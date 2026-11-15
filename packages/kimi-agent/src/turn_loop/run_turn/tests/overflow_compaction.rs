use super::super::*;
use super::common::*;
use super::*;

#[tokio::test]
async fn test_context_overflow_triggers_emergency_compaction_and_recovers() {
    struct OverflowRecoverLlm {
        call_count: AtomicU32,
    }
    impl LLM for OverflowRecoverLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if count == 0 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 400 Bad Request: context_length_exceeded",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else if count == 1 {
                    Ok(LLMChatResponse {
                        content: "Earlier user and assistant discussed the task.".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    assert_eq!(params.messages.len(), 5);
                    assert_eq!(params.messages[0].role, "system");
                    assert_eq!(params.messages[1].content, "u1\n\nu2\n\nu3");
                    assert!(
                        params.messages[2].content.starts_with(
                            "<system-reminder>
"
                        ),
                        "the reminder injected after the last user input is the in-flight turn"
                    );
                    assert!(
                        params.messages[3]
                            .content
                            .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX),
                        "the summary must ride the compaction prefix"
                    );
                    let summary_message = &params.messages[3];
                    assert!(
                        summary_message
                            .content
                            .contains("Earlier user and assistant discussed the task."),
                        "the generated summary must replace the omitted history"
                    );
                    assert!(
                        summary_message.content.starts_with("<system-reminder>")
                            && summary_message.content.ends_with("</system-reminder>"),
                        "v2 wraps the summary in a system-reminder"
                    );
                    assert_eq!(
                        params.messages[4].content,
                        crate::compaction::COMPACTION_CONTINUATION_TEXT,
                        "continuation note closes the compacted shape"
                    );
                    Ok(LLMChatResponse {
                        content: "Recovered successfully".into(),
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

    let llm = OverflowRecoverLlm {
        call_count: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    use crate::tools::external_hooks::capture;
    let Some(dir) = capture::dir() else {
        return;
    };
    let pre = dir.path().join("pre-compact.json");
    let post = dir.path().join("post-compact.json");
    let hook_guard = Some(Arc::new(crate::tools::external_hooks::HookGuard::new(
        vec![
            capture::hook("PreCompact", "auto", &pre),
            capture::hook("PostCompact", "auto", &post),
        ],
    )));

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: "test-overflow-recovery-agent".to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-recovery".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::Assistant,
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::Assistant,
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u3".into(),
                ..Default::default()
            },
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await.unwrap();
    assert_eq!(llm.call_count.load(Ordering::SeqCst), 3);
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);
    assert_eq!(
        result.messages.last().unwrap().content,
        "Recovered successfully"
    );
    let pre_payload = capture::wait(&pre).await;
    assert_eq!(pre_payload["hook_event_name"], "PreCompact");
    assert!(pre_payload["message_count"].as_u64().unwrap() > 0);
    let post_payload = capture::wait(&post).await;
    assert_eq!(post_payload["hook_event_name"], "PostCompact");
    assert_eq!(post_payload["trigger"], "auto");
    assert!(post_payload["estimated_token_count"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn test_compaction_attempt_cap_reaches_the_turn_loop_summarizer() {
    struct FailingSummarizerLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for FailingSummarizerLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "cap-model"
        }
        fn is_retryable_error(&self, error: &str) -> bool {
            error.contains("500")
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params.messages.last().is_some_and(|message| {
                message
                    .content
                    .contains("You are about to run out of context")
            });
            if is_summarizer {
                assert_eq!(
                    params
                        .messages
                        .first()
                        .map(|message| message.content.as_str()),
                    Some("sys"),
                    "the summarizer carries the agent's profile system prompt"
                );
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async move {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 500 summarizer unavailable",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                });
            }
            let step = self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = if step == 0 {
                "llm http status 400 Bad Request: context_length_exceeded"
            } else {
                "llm http status 401 Unauthorized: bad key"
            };
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = FailingSummarizerLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-cap".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::Assistant,
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::Assistant,
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: kimi_core::types::Role::User,
                content: "u3".into(),
                ..Default::default()
            },
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: Some(1),
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let error = run_turn(input, &callbacks).await.unwrap_err();
    assert!(
        error.to_string().contains("summarizer unavailable"),
        "{error}"
    );
    assert_eq!(
        llm.summarizer_calls.load(Ordering::SeqCst),
        1,
        "the configured cap bounds the summarizer requests"
    );
    assert_eq!(
        llm.step_calls.load(Ordering::SeqCst),
        1,
        "failed summarization must not retry a step with destroyed history"
    );
}

#[tokio::test]
async fn context_overflow_recovery_retries_the_step() {
    struct AlwaysOverflowLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for AlwaysOverflowLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params.messages.last().is_some_and(|message| {
                message
                    .content
                    .contains("You are about to run out of context")
            });
            let message = if is_summarizer {
                assert_eq!(
                    params
                        .messages
                        .first()
                        .map(|message| message.content.as_str()),
                    Some("sys"),
                    "the summarizer carries the agent's profile system prompt"
                );
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);

                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_other: 1,
                            output: 1,
                                                        ..Default::default()
                        },
                        timing: None,
                    })
                });
            } else {
                self.step_calls.fetch_add(1, Ordering::SeqCst);
                "llm http status 400 Bad Request: context_length_exceeded".to_string()
            };
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = AlwaysOverflowLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);

    let mut messages = vec![LLMMessage {
        role: kimi_core::types::Role::System,
        content: "sys".into(),
        ..Default::default()
    }];
    for index in 0..12 {
        messages.push(LLMMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index} with enough text to be worth compacting"),
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-retry".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    assert!(run_turn(input, &callbacks).await.is_err());
    let step_calls = llm.step_calls.load(Ordering::SeqCst);
    let rounds = llm.summarizer_calls.load(Ordering::SeqCst);

    assert!(
        rounds >= 2,
        "the overflow recovery must repeat, not give up after one round; \
             saw {rounds} round(s) and {step_calls} step call(s)"
    );
    assert!(
        rounds <= 3,
        "the recovery is bounded by maxOverflowCompactionAttempts (3); saw {rounds}"
    );
    assert_eq!(
        step_calls,
        rounds + 1,
        "each round retries the step exactly once"
    );
}

#[tokio::test]
async fn overflow_without_a_split_point_reports_cancelled_not_completed() {
    struct AlwaysOverflowLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for AlwaysOverflowLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "overflow-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params.messages.last().is_some_and(|message| {
                message
                    .content
                    .contains("You are about to run out of context")
            });
            if is_summarizer {
                assert_eq!(
                    params
                        .messages
                        .first()
                        .map(|message| message.content.as_str()),
                    Some("sys"),
                    "the summarizer carries the agent's profile system prompt"
                );
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_other: 1,
                            output: 1,
                                                        ..Default::default()
                        },
                        timing: None,
                    })
                });
            }
            self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = "llm http status 400 Bad Request: context_length_exceeded".to_string();
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = AlwaysOverflowLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let messages = vec![
        LLMMessage {
            role: kimi_core::types::Role::System,
            content: "sys".into(),
            ..Default::default()
        },
        LLMMessage {
            role: kimi_core::types::Role::User,
            content: "one oversized turn that cannot be split anywhere".into(),
            ..Default::default()
        },
    ];

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-no-split".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    assert!(run_turn(input, &callbacks).await.is_err());
    assert_eq!(
        llm.summarizer_calls.load(Ordering::SeqCst),
        0,
        "count == 0 must not reach the summarizer; if this is non-zero the \
             history has a split point and the test is not exercising the no-op"
    );

    let compaction: Vec<String> = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| event.get("type").and_then(|kind| kind.as_str()))
        .filter(|kind| kind.starts_with("compaction."))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        compaction,
        ["compaction.started", "compaction.cancelled"],
        "an emergency compaction that did nothing must close with a terminal \
             event saying so, not with a success card the turn then fails on"
    );
}

#[tokio::test]
async fn a_failing_summarizer_reports_compaction_failed() {
    struct FailingSummarizerLlm {
        summarizer_calls: AtomicU32,
        step_calls: AtomicU32,
    }
    impl LLM for FailingSummarizerLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "failing-summarizer-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let is_summarizer = params.messages.last().is_some_and(|message| {
                message
                    .content
                    .contains("You are about to run out of context")
            });
            if is_summarizer {
                assert_eq!(
                    params
                        .messages
                        .first()
                        .map(|message| message.content.as_str()),
                    Some("sys"),
                    "the summarizer carries the agent's profile system prompt"
                );
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);

                return Box::pin(async move {
                    Err(Box::new(std::io::Error::other("summarizer exploded"))
                        as Box<dyn std::error::Error + Send + Sync>)
                });
            }
            self.step_calls.fetch_add(1, Ordering::SeqCst);
            let message = "llm http status 400 Bad Request: context_length_exceeded".to_string();
            Box::pin(async move {
                Err(Box::new(std::io::Error::other(message))
                    as Box<dyn std::error::Error + Send + Sync>)
            })
        }
    }

    let llm = FailingSummarizerLlm {
        summarizer_calls: AtomicU32::new(0),
        step_calls: AtomicU32::new(0),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let mut messages = vec![LLMMessage {
        role: kimi_core::types::Role::System,
        content: "sys".into(),
        ..Default::default()
    }];
    for index in 0..12 {
        messages.push(LLMMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index} with enough text to be worth compacting"),
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-failed".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    assert!(run_turn(input, &callbacks).await.is_err());
    let asked = llm.summarizer_calls.load(Ordering::SeqCst);
    assert!(
        asked > 0,
        "the summarizer must have been asked; it was not, so this is not the failure path"
    );

    let recorded = events.lock().unwrap().clone();
    let failed: Vec<&serde_json::Value> = recorded
        .iter()
        .filter(|event| {
            event.get("event").and_then(|name| name.as_str()) == Some("compaction_failed")
        })
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "one compaction failed once; saw {recorded:?}"
    );

    assert_eq!(failed[0]["source"], "auto");
    assert!(failed[0]["tokens_before"].is_u64());
    assert!(failed[0]["duration_ms"].is_u64());
    assert_eq!(failed[0]["round"], 1, "the first overflow round");

    assert!(failed[0]["retry_count"].is_u64());
    let error_type = failed[0]["error_type"].as_str().unwrap_or_default();
    assert!(
        !error_type.is_empty(),
        "the failure class must be named: {failed:?}"
    );
    assert!(failed[0].get("reason").is_none());

    assert!(failed[0].get("turn_id").is_none());

    assert_eq!(failed[0]["thinking_effort"], "none");

    let terminal: Vec<String> = recorded
        .iter()
        .filter_map(|event| event.get("type").and_then(|kind| kind.as_str()))
        .filter(|kind| kind.starts_with("compaction."))
        .map(str::to_owned)
        .collect();
    assert_eq!(terminal, ["compaction.started", "compaction.cancelled"]);
}

#[tokio::test]
async fn a_too_large_request_degrades_then_strips_and_retries() {
    struct TooLargeLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
    }
    impl LLM for TooLargeLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "too-large-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentPart::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call < 2 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 413: request too large",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(TooLargeLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());

    let mut file_ids = Vec::new();
    for index in 0..4 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                &[
                    b"\x89PNG\r\n\x1a\n" as &[u8],
                    format!("PNGDATA-{index}").as_bytes(),
                ]
                .concat(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    let mut messages = vec![LLMMessage {
        role: kimi_core::types::Role::System,
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: kimi_core::types::Role::User,
            content: "look".into(),
            blocks: vec![crate::llm::media_resolver::file_media_part(
                file_id.clone(),
                crate::rpc::types::MediaKind::Image,
            )],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-too-large".into(),
        llm: llm.as_ref(),
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(
        result.is_ok(),
        "the stripped retry must succeed: {result:?}"
    );

    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 0],
        "the degrade keeps the two newest, the strip takes the rest"
    );
}

#[tokio::test]
async fn an_interruption_cancels_an_in_flight_media_upload() {
    struct UploadLlm {
        target: crate::llm::media_resolver::MediaTarget,
    }
    impl LLM for UploadLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "test-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn media_target(&self) -> Option<crate::llm::media_resolver::MediaTarget> {
            Some(self.target.clone())
        }
        fn media_upload_credential(
            &self,
        ) -> Option<crate::llm::files_upload::CredentialFuture<'_>> {
            Some(Box::pin(async { Ok("token".to_string()) }))
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            unreachable!("the upload never completes, so no chat request is built")
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (conn_tx, mut conn_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
            let _ = conn_tx.send(());
        }
    });

    let dir = std::env::temp_dir().join(format!("kimi-upload-cancel-{}", fastrand::u64(..)));
    let store = crate::server::files::FileStore::with_root(dir);
    let meta = store
        .save("clip.mp4", "video/mp4", None, b"MP4DATA")
        .unwrap();
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(store);
    let llm = UploadLlm {
        target: crate::llm::media_resolver::MediaTarget {
            protocol: "openai".into(),
            base_url: format!("http://{addr}"),
            provider_key: "kimi".into(),
            provider_type: "kimi".into(),
            image_in: true,
            video_in: true,
            audio_in: true,
            uploads_media: true,
            inline_video: false,
        },
    };
    let messages = vec![LLMMessage {
        role: kimi_core::types::Role::User,
        content: String::new(),
        blocks: vec![crate::llm::media_resolver::file_media_part(
            meta.id.clone(),
            crate::rpc::types::MediaKind::Video,
        )],
        ..Default::default()
    }];
    let server_rpc = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server_rpc);
    let mut budget = crate::llm::media_budget::MediaBudget::default();

    {
        let token = tokio_util::sync::CancellationToken::new();
        let call = budgeted_request(
            &mut budget,
            Some(&resolver),
            &llm,
            &messages,
            callbacks.as_ref(),
            Some(&token),
            u64::MAX,
            None,
        );
        tokio::pin!(call);
        tokio::select! {
            _ = conn_rx.recv() => {}
            _ = &mut call => panic!("the upload must still be in flight"),
        }
        token.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(5), &mut call)
            .await
            .expect("a cancelled upload must not be waited out")
            .unwrap();
        assert!(
            out.is_none(),
            "a cancelled resolve yields no request messages"
        );
    }

    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let out = budgeted_request(
        &mut budget,
        Some(&resolver),
        &llm,
        &messages,
        callbacks.as_ref(),
        Some(&token),
        u64::MAX,
        None,
    )
    .await
    .unwrap();
    assert!(out.is_none());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), conn_rx.recv())
            .await
            .is_err(),
        "a cancelled turn must not start an upload"
    );
    server.abort();
}

#[tokio::test]
async fn a_media_recovery_applies_to_later_turns_until_invalidated() {
    struct RecoveringLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
        model: std::sync::Mutex<String>,
    }
    impl LLM for RecoveringLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            let name = self.model.lock().unwrap().clone();
            Box::leak(name.into_boxed_str())
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentPart::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 413: request too large",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(RecoveringLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
        model: std::sync::Mutex::new("rec-model".to_string()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let mut file_ids = Vec::new();
    for index in 0..4 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                &[
                    b"\x89PNG\r\n\x1a\n" as &[u8],
                    format!("PNGDATA-{index}").as_bytes(),
                ]
                .concat(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);
    let recovery = crate::llm::media_budget::SharedMediaRecovery::default();

    let history = |prompt: &str| {
        let mut messages = vec![LLMMessage {
            role: kimi_core::types::Role::System,
            content: "sys".into(),
            ..Default::default()
        }];
        for file_id in &file_ids {
            messages.push(LLMMessage {
                role: kimi_core::types::Role::User,
                content: "look".into(),
                blocks: vec![crate::llm::media_resolver::file_media_part(
                    file_id.clone(),
                    crate::rpc::types::MediaKind::Image,
                )],
                ..Default::default()
            });
        }
        messages.push(LLMMessage {
            role: kimi_core::types::Role::User,
            content: prompt.into(),
            ..Default::default()
        });
        messages
    };
    let input_for = |messages: Vec<LLMMessage>, turn_id: &str| RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: turn_id.into(),
        llm: llm.as_ref(),
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: Some(recovery.clone()),
        toolset: None,
    };

    let result = run_turn(input_for(history("one"), "t1"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 2);
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2]);
    assert_eq!(
        recovery
            .lock()
            .unwrap()
            .recovery
            .as_ref()
            .map(|r| &r.action),
        Some(&crate::llm::media_budget::MediaRecoveryAction::Degraded(2)),
    );

    let result = run_turn(input_for(history("two"), "t2"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2],
        "the second turn must start degraded instead of re-sending 4 media"
    );

    *llm.model.lock().unwrap() = "other-model".to_string();
    let result = run_turn(input_for(history("three"), "t3"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4]);

    *llm.model.lock().unwrap() = "rec-model".to_string();
    let result = run_turn(input_for(history("four"), "t4"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4, 2]);

    recovery.lock().unwrap().advance_revision();
    let result = run_turn(input_for(history("five"), "t5"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2, 4, 2, 4]
    );
}

#[tokio::test]
async fn an_image_format_rejection_strips_the_media_and_retries() {
    struct ImageFormatLlm {
        step_calls: AtomicU32,
        seen_media: std::sync::Mutex<Vec<usize>>,
    }
    impl LLM for ImageFormatLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "image-format-model"
        }
        fn is_retryable_error(&self, _error: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let media = params
                .messages
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter(|block| !matches!(block, crate::rpc::types::ContentPart::Text { .. }))
                .count();
            self.seen_media.lock().unwrap().push(media);
            let call = self.step_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if call == 0 {
                    Err(Box::new(std::io::Error::other(
                        "llm http status 400: unsupported image format image/heic",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else {
                    Ok(LLMChatResponse {
                        content: "ok".into(),
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

    let llm = Arc::new(ImageFormatLlm {
        step_calls: AtomicU32::new(0),
        seen_media: std::sync::Mutex::new(Vec::new()),
    });
    let server = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server);
    let dir = tempfile::tempdir().unwrap();
    let files = crate::server::files::FileStore::with_root(dir.path().to_path_buf());
    let mut file_ids = Vec::new();
    for index in 0..3 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                &[
                    b"\x89PNG\r\n\x1a\n" as &[u8],
                    format!("PNGDATA-{index}").as_bytes(),
                ]
                .concat(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    let mut messages = vec![LLMMessage {
        role: kimi_core::types::Role::System,
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: kimi_core::types::Role::User,
            content: "look".into(),
            blocks: vec![crate::llm::media_resolver::file_media_part(
                file_id.clone(),
                crate::rpc::types::MediaKind::Image,
            )],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-image-format".into(),
        llm: llm.as_ref(),
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: Some(&resolver),
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks).await;
    assert!(
        result.is_ok(),
        "the stripped retry must succeed: {result:?}"
    );

    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 2);
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[3, 0]);
}

#[tokio::test]
async fn test_over_budget_media_are_omitted_from_the_request_and_warned_about() {
    use std::sync::Mutex;

    struct CaptureLlm {
        captured: Arc<Mutex<Vec<LLMMessage>>>,
    }
    impl LLM for CaptureLlm {
        fn system_prompt(&self) -> &str {
            "base prompt"
        }
        fn model_name(&self) -> &str {
            "capture"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let captured = self.captured.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = params.messages.to_vec();
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_other: 5,
                        output: 3,
                                                ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let image = |tag: &str, bytes: usize| LLMMessage {
        role: kimi_core::types::Role::User,
        content: String::new(),
        blocks: vec![crate::llm::media_resolver::inline_image_part(
            "image/png".into(),
            format!("{tag}{}", "A".repeat(bytes)),
            None,
        )],
        tool_calls: Vec::new(),
        tool_call_id: None,
        tools: Vec::new(),

        prompt_id: None,
        origin: None,
    };
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-media-budget".into(),
        llm: &llm,
        messages: vec![
            image("a", 8 * 1024 * 1024),
            image("b", 8 * 1024 * 1024),
            image("c", 8 * 1024 * 1024),
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 1,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let turn = run_turn(input, &callbacks).await.unwrap();

    let sent = captured.lock().unwrap().clone();
    let omitted_block = |block: &ContentPart| {
        matches!(
            block,
            ContentPart::Text { text, .. }
                if text == "[image omitted: dropped to fit the request media budget]"
        )
    };
    let omitted = |message: &LLMMessage| message.blocks.iter().any(omitted_block);

    let media: Vec<&ContentPart> = sent
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter(|b| omitted_block(b) || matches!(b, ContentPart::ImageUrl { .. }))
        .collect();
    assert_eq!(media.len(), 3, "{sent:?}");
    assert!(
        omitted_block(media[0]),
        "the oldest media leaves the request"
    );
    assert!(omitted_block(media[1]));
    assert!(
        matches!(media[2], ContentPart::ImageUrl { .. }),
        "the newest media stays: {:?}",
        media[2]
    );
    assert!(
        turn.messages
            .iter()
            .filter(|m| m.role == "user")
            .all(|m| !omitted(m)),
        "the history keeps every block, so a later request can still see them"
    );
    let warnings: Vec<serde_json::Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.get("type").and_then(|t| t.as_str()) == Some("warning"))
        .cloned()
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["code"], "media-budget-exceeded");
    assert_eq!(
        warnings[0]["message"],
        "Conversation media exceeded the 20 MB per-request budget; \
             2 older media item(s) were omitted."
    );
}

#[tokio::test]
async fn a_request_over_the_byte_budget_evicts_media_before_sending() {
    use std::sync::Mutex;

    struct CaptureLlm {
        captured: Arc<Mutex<Vec<LLMMessage>>>,
    }
    impl LLM for CaptureLlm {
        fn system_prompt(&self) -> &str {
            "base prompt"
        }
        fn model_name(&self) -> &str {
            "capture"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn chat(
            &self,
            params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let captured = self.captured.clone();
            Box::pin(async move {
                *captured.lock().unwrap() = params.messages.to_vec();
                Ok(LLMChatResponse {
                    content: String::new(),
                    thinking: vec![],
                    tool_calls: vec![],
                    finish_reason: Some("stop".into()),
                    usage: TokenUsage {
                        input_other: 5,
                        output: 3,
                                                ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let image = |tag: &str, bytes: usize| LLMMessage {
        role: kimi_core::types::Role::User,
        content: String::new(),
        blocks: vec![crate::llm::media_resolver::inline_image_part(
            "image/png".into(),
            format!("{tag}{}", "A".repeat(bytes)),
            None,
        )],
        tool_calls: Vec::new(),
        tool_call_id: None,
        tools: Vec::new(),

        prompt_id: None,
        origin: None,
    };
    let captured: Arc<Mutex<Vec<LLMMessage>>> = Arc::new(Mutex::new(Vec::new()));
    let llm = CaptureLlm {
        captured: captured.clone(),
    };
    let recovery: Arc<Mutex<kimi_core::types::MediaRecoveryState>> = Arc::new(Mutex::new(
        kimi_core::types::MediaRecoveryState::default(),
    ));
    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);

    let input = RunTurnInput {
        llm_request_byte_budget: 20 * 1024 * 1024,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-request-byte-budget".into(),
        llm: &llm,
        messages: vec![
            image("a", 8 * 1024 * 1024),
            image("b", 8 * 1024 * 1024),
            image("c", 8 * 1024 * 1024),
        ],
        tools: &[],
        tool_defs: vec![],
        max_steps: 1,
        max_attempts: None,
        max_context_tokens: None,
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: Some(recovery.clone()),
        toolset: None,
    };

    run_turn(input, &callbacks).await.unwrap();

    assert!(
        recovery.lock().unwrap().applicable("capture").is_some(),
        "the proactive eviction is recorded so the next turn keeps it"
    );

    let sent = captured.lock().unwrap().clone();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert!(
        matches!(
            &sent[1].blocks[0],
            ContentPart::Text { text, .. } if text == "[image omitted: dropped to fit the provider request size limit; re-read the file to view it]"
        ),
        "the oldest media is evicted before send: {:?}",
        sent[1].blocks[0]
    );
    assert!(matches!(sent[1].blocks[1], ContentPart::ImageUrl { .. }));
    assert!(matches!(sent[1].blocks[2], ContentPart::ImageUrl { .. }));
    let warnings: Vec<serde_json::Value> = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.get("type").and_then(|t| t.as_str()) == Some("warning"))
        .cloned()
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["code"], "media-degraded");
}

#[tokio::test]
async fn a_successful_overflow_compaction_reports_compaction_finished() {
    struct UsageSummarizerLlm {
        calls: AtomicU32,
    }
    impl LLM for UsageSummarizerLlm {
        fn system_prompt(&self) -> &str {
            "sys"
        }
        fn model_name(&self) -> &str {
            "finished-model"
        }
        fn is_retryable_error(&self, _: &str) -> bool {
            false
        }
        fn transport(&self) -> &'static str {
            "native-http"
        }
        fn chat(
            &self,
            _params: LLMChatParams,
        ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>>
        {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                match call {
                    0 => Err(Box::new(std::io::Error::other(
                        "llm http status 400 Bad Request: context_length_exceeded",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>),
                    1 => Ok(LLMChatResponse {
                        content: "Summary of the omitted prefix.".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_other: 7,
                            output: 3,
                            input_cache_read: 2,
                            input_cache_creation: 1,
                            raw: None,
                        },
                        timing: None,
                    }),
                    _ => Ok(LLMChatResponse {
                        content: "Recovered".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    }),
                }
            })
        }
    }

    let llm = UsageSummarizerLlm {
        calls: AtomicU32::new(0),
    };

    let mut messages = vec![LLMMessage {
        role: kimi_core::types::Role::System,
        content: "sys".into(),
        ..Default::default()
    }];
    for index in 0..12 {
        messages.push(LLMMessage {
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("turn {index} with enough text to be worth compacting"),
            ..Default::default()
        });
    }

    let server = Arc::new(RpcServer::new());
    let (capturing, events) = EventCapturingCallbacks::new(rpc_callbacks(server.clone()));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(capturing);
    let input = RunTurnInput {
        llm_request_byte_budget: u64::MAX,
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-finished".into(),
        llm: &llm,
        messages,
        tools: &[],
        tool_defs: vec![],
        max_steps: 5,
        max_attempts: None,
        max_context_tokens: Some(100_000),
        compaction_max_attempts: None,
        permission_mode: None,
        goal: None,
        cancellation: None,
        hook_guard: None,
        media: None,
        media_dropped: None,
        media_recovery: None,
        toolset: None,
    };

    let result = run_turn(input, &callbacks)
        .await
        .expect("the turn recovers");
    assert_eq!(result.stop_reason, LoopTurnStopReason::EndTurn);

    let recorded = events.lock().unwrap().clone();
    let finished: Vec<&serde_json::Value> = recorded
        .iter()
        .filter(|event| {
            event.get("event").and_then(|name| name.as_str()) == Some("compaction_finished")
        })
        .collect();
    assert_eq!(
        finished.len(),
        1,
        "one compaction completed; saw {recorded:?}"
    );
    let payload = finished[0];
    assert_eq!(payload["source"], "auto");
    assert_eq!(payload["round"], 1);
    assert_eq!(payload["retry_count"], 0);

    assert!(payload["tokens_before"].as_u64().unwrap() > 0);
    assert!(payload["tokens_after"].as_u64().unwrap() > 0);
    assert!(payload["compacted_count"].as_u64().unwrap() > 0);
    assert!(payload["duration_ms"].is_u64());
    assert_eq!(payload["thinking_effort"], "none");

    assert_eq!(payload["input_tokens"], 10);
    assert_eq!(payload["output_tokens"], 3);
    assert_eq!(payload["input_cache_read"], 2);
    assert_eq!(payload["input_cache_creation"], 1);
}

#[test]
fn the_in_turn_gauge_rebases_on_the_measured_summary_output() {
    let summary = LLMMessage {
        role: kimi_core::types::Role::User,
        content: format!(
            "{} the summary body",
            crate::compaction::COMPACTION_SUMMARY_PREFIX
        ),
        ..Default::default()
    };
    let history = vec![
        LLMMessage {
            role: kimi_core::types::Role::System,
            content: "system prompt".into(),
            ..Default::default()
        },
        summary,
        LLMMessage {
            role: kimi_core::types::Role::Assistant,
            content: "kept reply".into(),
            ..Default::default()
        },
    ];
    let estimate = crate::compaction::estimate_messages_tokens(&history);
    let summary_estimate = crate::compaction::estimate_message_tokens(&history[1]);

    let mut gauge = ContextTokens::default();
    gauge.rebase_with_summary(&history, None);
    assert_eq!(
        gauge.tokens(),
        estimate,
        "without a measurement the estimate stands"
    );

    gauge.rebase_with_summary(&history, Some(1));
    assert_eq!(
        gauge.tokens(),
        estimate - summary_estimate + 1,
        "the measured output replaces the summary's estimated size"
    );

    gauge.sync(&history);
    assert_eq!(
        gauge.tokens(),
        estimate - summary_estimate + 1,
        "a sync with no new messages keeps the measured figure"
    );
}
