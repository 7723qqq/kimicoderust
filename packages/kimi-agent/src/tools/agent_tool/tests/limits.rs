use super::super::*;
use super::common::*;
use super::*;

/// A finish_reason of `length` maps to a MaxTokens stop — v2 fails the
/// subagent run with a verbatim error instead of reporting a summary.
struct MaxTokensLlm;
impl LLM for MaxTokensLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "max-tokens-llm"
    }
    fn is_retryable_error(&self, _: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _: LLMChatParams,
    ) -> BoxFuture<'_, Result<TurnChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            Ok(TurnChatResponse {
                content: "partial".into(),
                thinking: vec![],
                tool_calls: vec![],
                finish_reason: Some("length".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

#[tokio::test]
async fn max_tokens_truncation_fails_the_subagent_run() {
    let manager = manager_with(Arc::new(MaxTokensLlm)).await;
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
        &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("truncation is a native outcome, not a fallback");
    assert!(result.is_error);
    assert!(
        result
            .content
            .contains("Subagent turn failed before completing its final summary"),
        "v2 max_tokens error text: {}",
        result.content
    );
}

#[tokio::test]
async fn resume_turns_distill_under_the_same_policy() {
    let _owned = as_main_agent(async {
        let recorder = Arc::new(EventRecorder::new());
        let llm = Arc::new(RecordingPromptLlm::new(vec![
            "first pass summary that is long enough".into(),
            "short".into(),
            "follow-up answer with plenty of detail".into(),
        ]));
        let manager = manager_with_callbacks(llm.clone(), recorder.clone()).await;
        manager
            .register_discovered_definition(crate::subagent::types::SubagentDefinition {
                name: "research".into(),
                description: "d".into(),
                system_prompt: "You research.".into(),
                tools: vec![],
                disallowed_tools: vec![],
                when_to_use: None,
                prompt_prefix: None,
                summary_policy: Some(SummaryPolicy {
                    min_chars: 20,
                    continuation_prompt: "Summarize fully.".into(),
                    retries: 1,
                }),
                model: None,
                subagents: None,
            })
            .await;
        let first = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "subagent_type": "research", "prompt": "go" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("first turn runs natively");
        assert!(!first.is_error);
        let agent_id = recorder.events.lock().unwrap()[0]["subagent_id"]
            .as_str()
            .unwrap()
            .to_string();

        let second = execute_agent(
            &manager,
            None,
            &serde_json::json!({ "resume": agent_id, "prompt": "more" }),
            None,
            None,
            None,
            None,
        )
        .await
        .expect("resume runs natively");
        assert!(!second.is_error);
        // three LLM calls: initial, resume turn, distillation continuation
        assert_eq!(llm.call_count(), 3);
        assert!(
            second
                .content
                .contains("follow-up answer with plenty of detail"),
            "the distilled continuation becomes the resume summary: {}",
            second.content
        );
    })
    .await;
}

#[test]
fn timeout_description_mirrors_v2() {
    assert_eq!(format_timeout_description(2 * 60 * 60 * 1000), "2 hours");
    assert_eq!(format_timeout_description(60 * 60 * 1000), "1 hour");
    assert_eq!(format_timeout_description(5 * 60 * 1000), "5 minutes");
    assert_eq!(format_timeout_description(90 * 1000), "90 seconds");
    assert_eq!(format_timeout_description(1500), "1500 ms");
}

#[tokio::test]
async fn foreground_success_formats_v2_shape() {
    let manager = manager_with(Arc::new(SummaryLlm)).await;
    // `research` is not in the default allowlist; register it as a
    // discovered profile so the spawn gate admits it (v2 extras).
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
            "prompt": "find the loop",
            "description": "Loop finder"
        }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("foreground call must run natively");
    assert!(!result.is_error);
    let content = result.content;
    assert!(content.contains("actual_subagent_type: research"));
    assert!(content.contains("status: completed"));
    assert!(content.contains("[summary]\nfindings: the loop is in run_turn.rs"));
    assert!(content.starts_with("agent_id: subagent-"));
}

#[tokio::test]
async fn default_profile_is_coder() {
    let manager = manager_with(Arc::new(SummaryLlm)).await;
    // The snapshot pushes `coder`; untyped calls must resolve to it.
    manager
        .register_definition(crate::subagent::types::SubagentDefinition {
            name: "coder".into(),
            description: "default".into(),
            system_prompt: "You code.".into(),
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
        &serde_json::json!({ "prompt": "do it", "description": "x" }),
        None,
        None,
        None,
        None,
    )
    .await
    .expect("untyped call resolves to the coder profile");
    assert!(result.content.contains("actual_subagent_type: coder"));
}

#[tokio::test]
async fn unknown_profile_falls_back_to_host() {
    let manager = manager_with(Arc::new(SummaryLlm)).await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "plugin-reviewer", "prompt": "x" }),
        None,
        None,
        None,
        None,
    )
    .await;
    assert!(result.is_none(), "unknown profiles stay host-owned");
}

#[tokio::test]
async fn extended_features_fall_back_to_host() {
    let manager = manager_with(Arc::new(SummaryLlm)).await;
    // Unknown resume ids stay host-owned (P55 native resume only covers
    // agents whose conversation we hold; unknown ids fall back so the
    // host's persistent scopes can resolve them).
    assert!(
        execute_agent(
            &manager,
            None,
            &serde_json::json!({ "resume": "agent-9", "prompt": "x" }),
            None,
            None,
            None,
            None,
        )
        .await
        .is_none(),
        "unknown resume ids must fall back to the host"
    );
    // Note: `run_in_background` (P58), `resume`, `fork` and the explicit
    // `model` override are all native; the spawn paths cover them.
}

#[tokio::test]
async fn timeout_reports_v2_failure_shape() {
    let manager = manager_with(Arc::new(ToolForeverLlm)).await;
    register_looper(&manager).await;
    let result = execute_agent(
        &manager,
        None,
        &serde_json::json!({ "subagent_type": "looper", "prompt": "loop forever" }),
        Some(300),
        None,
        None,
        None,
    )
    .await
    .expect("timeout is a native outcome, not a fallback");
    assert!(result.is_error);
    assert!(result.content.contains("status: failed"));
    assert!(result.content.contains("Agent timed out after 300 ms."));
    assert!(result.content.contains("resume_hint:"));
}
