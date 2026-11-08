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
                    // First call fails with a context overflow error.
                    Err(Box::new(std::io::Error::other(
                        "llm http status 400 Bad Request: context_length_exceeded",
                    ))
                        as Box<dyn std::error::Error + Send + Sync>)
                } else if count == 1 {
                    // The summarizer produces the replacement text.
                    Ok(LLMChatResponse {
                        content: "Earlier user and assistant discussed the task.".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage::default(),
                        timing: None,
                    })
                } else {
                    // Third call succeeds after compaction. v2 shape:
                    // system + head user input (u1, kept verbatim) +
                    // elision note + prefixed summary + continuation +
                    // the uncompacted tail (u2/a2/u3) + date reminder.
                    assert_eq!(params.messages.len(), 8);
                    assert_eq!(params.messages[0].role, "system");
                    assert_eq!(params.messages[1].content, "u1");
                    // The compacted range here covers only u1/a1 — its
                    // head keeps u1 verbatim; u2/u3 live in the
                    // uncompacted tail.
                    assert_eq!(params.messages[2].role, "user");
                    assert!(
                        params.messages[2]
                            .content
                            .contains(crate::compaction::COMPACTION_SUMMARY_PREFIX),
                        "the summary must ride the compaction prefix"
                    );
                    let summary_message = &params.messages[2];
                    assert!(
                        summary_message
                            .content
                            .ends_with("Earlier user and assistant discussed the task."),
                        "the generated summary must replace the omitted history"
                    );
                    assert_eq!(
                        params.messages[3].content,
                        crate::compaction::COMPACTION_CONTINUATION_TEXT,
                        "continuation note carried by the compacted shape, ahead of the tail"
                    );
                    assert_eq!(params.messages[4].content, "u2");
                    assert_eq!(params.messages[5].content, "a2");
                    assert_eq!(params.messages[6].content, "u3");
                    assert!(
                        params.messages[7].content.starts_with(
                            "<system-reminder>
"
                        ),
                        "date reminder appended after emergency compaction"
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

    // The compaction pair rides the same input: PreCompact carries the
    // message count before the trim, PostCompact the trigger and the
    // post-trim token count. Removing either dispatch site inside the
    // overflow-recovery arm leaves its capture file empty.
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-overflow-recovery".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
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

/// v2 #3750: the host's `loopControl.compactionMaxAttempts` reaches the
/// summarizer through the turn loop, so a failing emergency compaction
/// stops at the configured request count instead of the engine default.
///
/// The step overflows exactly once and then fails for an unrelated reason,
/// so exactly one compaction round runs and the per-round cap is the only
/// thing that could produce a second summarizer request.
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
            // Only the summarizer's 500 is retryable; the step errors must
            // fail straight through so the test does not sit in the
            // step-retry backoff.
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
            // The summarizer prompt is the only call whose system message
            // is the summarizer instruction; the step call carries the
            // turn's own system prompt.
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
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
                // First step: a context overflow, so the emergency
                // compaction path runs.
                "llm http status 400 Bad Request: context_length_exceeded"
            } else {
                // The retried step fails for a reason compaction cannot
                // help with, ending the turn after one round.
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
        agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
        previous_turn_aborted: false,
        turn_id: "test-compaction-cap".into(),
        llm: &llm,
        messages: vec![
            LLMMessage {
                role: "user".into(),
                content: "u1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a1".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
                content: "u2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "assistant".into(),
                content: "a2".into(),
                ..Default::default()
            },
            LLMMessage {
                role: "user".into(),
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

/// v2 `recoverFromContextOverflow` (fullCompactionService.ts:468-478)
/// compacts and **retries the step**, allowing
/// `maxOverflowCompactionAttempts` (3) consecutive rounds before the turn
/// fails. Recovering exactly once — what the loop used to do — fails a turn
/// v2 recovers on its second or third attempt.
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
            // Neither failure is retried by the step layer, so the only
            // repetition this test can observe is the overflow recovery.
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
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            let message = if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                // Summarise successfully so the compaction actually shrinks
                // the history and the next round has something to do.
                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_tokens: 1,
                            output_tokens: 1,
                            total_tokens: 2,
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

    // A history long enough that a compaction still has a prefix to remove
    // on the second round.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
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
    // Each recovery round compacts (one summarizer request) and retries the
    // step, so the rounds are what distinguishes this from the old
    // single-shot recovery: that one produced exactly one round.
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

/// The emergency path used to announce `compaction.completed` and only
/// then decide the round had done nothing. `count == 0` makes
/// `force_compact_messages_with_summary_report` answer with a bare no-op —
/// no summarizer request, the messages untouched,
/// `tokensAfter == tokensBefore` — so that card claimed a success for zero
/// work and the turn failed a line later anyway.
///
/// v2 gets the same outcome from the other side: `fullCompactionService`
/// throws on an empty summary inside `compactionRound`, so
/// `compactionWorker` never reaches its `CompactionCompleted` dispatch and
/// the `catch` lands on `cancelActive` -> `CompactionCancelled`. The turn
/// loop already owes a terminal event once `compaction.started` is out, so
/// the question is only which one; `cancelled` is the one that matches
/// what happened.
///
/// `[system, single user]` is what drives `count` to 0:
/// `can_split_after` refuses to cut after a `user` message, so the split
/// search finds no candidate and `fit_compact_count_to_window(0)` floors
/// the count.
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
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                return Box::pin(async move {
                    Ok(LLMChatResponse {
                        content: "summary of the omitted prefix".into(),
                        thinking: vec![],
                        tool_calls: vec![],
                        finish_reason: Some("stop".into()),
                        usage: TokenUsage {
                            input_tokens: 1,
                            output_tokens: 1,
                            total_tokens: 2,
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
            role: "system".into(),
            content: "sys".into(),
            ..Default::default()
        },
        LLMMessage {
            role: "user".into(),
            content: "one oversized turn that cannot be split anywhere".into(),
            ..Default::default()
        },
    ];

    let input = RunTurnInput {
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

/// A summarizer that fails outright is a different event from one that
/// produces nothing, and until now nothing observed which one was emitted:
/// renaming the `compaction_failed` telemetry event left every test in this
/// module green. The event name is the whole payload a consumer routes on,
/// so an unobserved name is an unobserved event.
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
            let is_summarizer = params
                .messages
                .first()
                .is_some_and(|message| message.content.contains("conversation summarizer"));
            if is_summarizer {
                self.summarizer_calls.fetch_add(1, Ordering::SeqCst);
                // The failure the emergency path must report, as opposed to
                // an empty summary, which is a no-op with its own event.
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

    // A history with a split point, so the emergency path actually asks the
    // summarizer instead of flooring the count to zero.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
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
    // v2's `CompactionFailedEvent` property names and required set. `reason`
    // and a string `turn_id` were this engine's invention: v2 has no
    // `reason` field, its `turn_id` is a numeric per-agent turn index, and
    // its `error_type` is the class name.
    assert_eq!(failed[0]["source"], "auto");
    assert!(failed[0]["tokens_before"].is_u64());
    assert!(failed[0]["duration_ms"].is_u64());
    assert_eq!(failed[0]["round"], 1, "the first overflow round");
    // `retry_count` is the per-round budget the config allows, which is what
    // v2 reports for the round that ran out of retries.
    assert!(failed[0]["retry_count"].is_u64());
    let error_type = failed[0]["error_type"].as_str().unwrap_or_default();
    assert!(
        !error_type.is_empty(),
        "the failure class must be named: {failed:?}"
    );
    assert!(failed[0].get("reason").is_none());
    // `turn_id` is v2's numeric per-agent index. This test's wire id carries
    // none (`test-compaction-cap`), so the property is absent rather than a
    // string standing in for a number.
    assert!(failed[0].get("turn_id").is_none());
    // `thinking_effort` is required by v2; the stub transport reports no
    // effort, and `none` is the wire's own spelling for that.
    assert_eq!(failed[0]["thinking_effort"], "none");

    // The terminal card still goes out: the turn owes one once
    // compaction.started has been emitted.
    let terminal: Vec<String> = recorded
        .iter()
        .filter_map(|event| event.get("type").and_then(|kind| kind.as_str()))
        .filter(|kind| kind.starts_with("compaction."))
        .map(str::to_owned)
        .collect();
    assert_eq!(terminal, ["compaction.started", "compaction.cancelled"]);
}

/// v2 `nextProjectionPolicyForError`: a provider "request too large"
/// degrades the older media and retries the step; a second rejection
/// strips every media part. The step layer retries neither, so the
/// repetition this test observes is the projection fallback's.
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
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
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
    // Real blobs, so the resolver inlines them instead of degrading the
    // reference to its unavailable placeholder. The saved ids are what
    // the messages reference.
    let mut file_ids = Vec::new();
    for index in 0..4 {
        let meta = files
            .save(
                &format!("img-{index}.png"),
                "image/png",
                None,
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    // Four media references across the history: the degrade keeps the
    // two newest, the strip takes the rest.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: "user".into(),
            content: "look".into(),
            blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                file_id: file_id.clone(),
                kind: crate::rpc::types::MediaKind::Image,
            }],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
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
    // Three step calls: the original, the degraded retry, the stripped
    // retry — and the media count falls 4 → 2 → 0.
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 0],
        "the degrade keeps the two newest, the strip takes the rest"
    );
}

/// v2 #4183: interrupting the turn cancels an in-flight media upload — the
/// resolve future is dropped, taking the upload's HTTP request with it —
/// and an already-cancelled turn never starts one at all.
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

    // A server that reports each connection and never answers: the upload
    // stays in flight until the client gives up on it. The sockets are held
    // so accepting one does not close it.
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
            image_in: true,
            video_in: true,
            audio_in: true,
            uploads_media: true,
        },
    };
    let messages = vec![LLMMessage {
        role: "user".into(),
        content: String::new(),
        blocks: vec![crate::turn_loop::types::ContentBlock::MediaRef {
            file_id: meta.id.clone(),
            kind: crate::rpc::types::MediaKind::Video,
        }],
        ..Default::default()
    }];
    let server_rpc = Arc::new(RpcServer::new());
    let callbacks = rpc_callbacks(server_rpc);
    let mut budget = crate::llm::media_budget::MediaBudget::default();

    // Mid-upload cancellation: the resolve returns promptly with nothing,
    // instead of the turn waiting the upload out.
    {
        let token = tokio_util::sync::CancellationToken::new();
        let call = budgeted_request(
            &mut budget,
            Some(&resolver),
            &llm,
            &messages,
            callbacks.as_ref(),
            Some(&token),
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

    // An already-cancelled token never starts the upload at all (v2's
    // `throwIfAborted` at entry).
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let out = budgeted_request(
        &mut budget,
        Some(&resolver),
        &llm,
        &messages,
        callbacks.as_ref(),
        Some(&token),
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

/// v2 #4413: a media recovery the provider forced on one turn is agent-level
/// state — the next turn starts from the recovered projection instead of
/// re-sending the rejected media, and a history rewrite (undo) or a model
/// switch invalidates it.
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
            // The Mutex guard cannot escape; leak once per switch — tests run
            // a handful of switches at most.
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
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
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
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);
    let recovery = crate::llm::media_budget::SharedMediaRecovery::default();

    // The session history keeps the original references; each turn gets a
    // fresh copy (production re-reads the store per turn).
    let history = |prompt: &str| {
        let mut messages = vec![LLMMessage {
            role: "system".into(),
            content: "sys".into(),
            ..Default::default()
        }];
        for file_id in &file_ids {
            messages.push(LLMMessage {
                role: "user".into(),
                content: "look".into(),
                blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                    file_id: file_id.clone(),
                    kind: crate::rpc::types::MediaKind::Image,
                }],
                ..Default::default()
            });
        }
        messages.push(LLMMessage {
            role: "user".into(),
            content: prompt.into(),
            ..Default::default()
        });
        messages
    };
    let input_for = |messages: Vec<LLMMessage>, turn_id: &str| RunTurnInput {
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

    // Turn 1: the 413 fires the degrade (4 → 2) and records the recovery.
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

    // Turn 2: the recovery applies before the first request — no rejection
    // needed, the projection is already degraded.
    let result = run_turn(input_for(history("two"), "t2"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 3);
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2],
        "the second turn must start degraded instead of re-sending 4 media"
    );

    // A model switch invalidates the recovery (v2 keys it by model alias).
    *llm.model.lock().unwrap() = "other-model".to_string();
    let result = run_turn(input_for(history("three"), "t3"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4]);

    // Back on the original model the recovery applies again (it was keyed
    // for it and never invalidated by the switch).
    *llm.model.lock().unwrap() = "rec-model".to_string();
    let result = run_turn(input_for(history("four"), "t4"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[4, 2, 2, 4, 2]);

    // A history rewrite (undo / set_history / clear_history) advances the
    // revision and invalidates the recovery.
    recovery.lock().unwrap().advance_revision();
    let result = run_turn(input_for(history("five"), "t5"), &callbacks).await;
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        llm.seen_media.lock().unwrap().as_slice(),
        &[4, 2, 2, 4, 2, 4]
    );
}

/// v2 `nextProjectionPolicyForError`'s image-format arm
/// (`isImageFormatError`): the provider rejects an image, and the
/// request is retried with every media part stripped — no degrade
/// round, the way v2 gives it none.
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
                .filter(|block| !matches!(block, crate::rpc::types::ContentBlock::Text { .. }))
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
                format!("PNGDATA-{index}").as_bytes(),
            )
            .unwrap();
        file_ids.push(meta.id);
    }
    let resolver = crate::llm::media_resolver::MediaResolver::with_store(files);

    let mut messages = vec![LLMMessage {
        role: "system".into(),
        content: "sys".into(),
        ..Default::default()
    }];
    for file_id in &file_ids {
        messages.push(LLMMessage {
            role: "user".into(),
            content: "look".into(),
            blocks: vec![crate::rpc::types::ContentBlock::MediaRef {
                file_id: file_id.clone(),
                kind: crate::rpc::types::MediaKind::Image,
            }],
            ..Default::default()
        });
    }

    let input = RunTurnInput {
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
    // Two step calls, and the media goes straight to zero — the
    // image-format arm has no degrade round.
    assert_eq!(llm.step_calls.load(Ordering::SeqCst), 2);
    assert_eq!(llm.seen_media.lock().unwrap().as_slice(), &[3, 0]);
}

/// The media budget degrades an over-budget request instead of failing it:
/// the oldest media leave the request, the history keeps them, and the
/// omission reaches the user as a `warning` event.
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
                        input_tokens: 5,
                        output_tokens: 3,
                        total_tokens: 8,
                        ..Default::default()
                    },
                    timing: None,
                })
            })
        }
    }

    let image = |tag: &str, bytes: usize| LLMMessage {
        role: "user".into(),
        content: String::new(),
        blocks: vec![ContentBlock::Image {
            media_type: "image/png".into(),
            data: format!("{tag}{}", "A".repeat(bytes)),
            name: None,
        }],
        tool_calls: Vec::new(),
        tool_call_id: None,

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
    let omitted = |message: &LLMMessage| {
        matches!(
            message.blocks.first(),
            Some(ContentBlock::Text { text, .. })
                if text == "[image omitted: dropped to fit the request media budget]"
        )
    };
    // The turn head is the rebuilt system message, and the loop injects a
    // date reminder of its own — the media are the messages that carry a
    // media block or the placeholder one left behind.
    let media: Vec<&LLMMessage> = sent
        .iter()
        .filter(|m| {
            omitted(m)
                || m.blocks
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Image { .. }))
        })
        .collect();
    assert_eq!(media.len(), 3, "{sent:?}");
    assert!(omitted(media[0]), "the oldest media leaves the request");
    assert!(omitted(media[1]));
    assert!(
        !omitted(media[2]),
        "the newest media stays: {:?}",
        media[2].blocks
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

/// v2 `compaction_finished` (`fullCompactionService.ts:768-795`): a successful
/// compaction reports the round's telemetry — the summarization request's
/// usage included — and not only the failure event. Before this, a successful
/// compaction produced no telemetry at all, so the cost of the summarizer call
/// was invisible.
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
                            input_tokens: 7,
                            output_tokens: 3,
                            total_tokens: 10,
                            input_cache_read: 2,
                            input_cache_creation: 1,
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
    // A history with a split point, so the emergency path actually asks the
    // summarizer instead of flooring the count to zero.
    let mut messages = vec![LLMMessage {
        role: "system".into(),
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
    // v2 reports the measured pair; it does not promise the projection
    // shrinks (a short history plus a long summary can grow).
    assert!(payload["tokens_before"].as_u64().unwrap() > 0);
    assert!(payload["tokens_after"].as_u64().unwrap() > 0);
    assert!(payload["compacted_count"].as_u64().unwrap() > 0);
    assert!(payload["duration_ms"].is_u64());
    assert_eq!(payload["thinking_effort"], "none");
    // v2 `usageTelemetry`: input_tokens is the *total* input (uncached plus
    // both cache buckets), and the request that succeeded is the one reported.
    assert_eq!(payload["input_tokens"], 10);
    assert_eq!(payload["output_tokens"], 3);
    assert_eq!(payload["input_cache_read"], 2);
    assert_eq!(payload["input_cache_creation"], 1);
}

/// v2 `applyCompaction`'s `tokensAfter` in the turn loop's own gauge: after a
/// compaction the gauge takes the summary's measured output instead of
/// re-estimating its text, so the next threshold decision is not driven by a
/// guess. A later `sync` with no new messages keeps the measured figure.
#[test]
fn the_in_turn_gauge_rebases_on_the_measured_summary_output() {
    let summary = LLMMessage {
        role: "user".into(),
        content: format!(
            "{} the summary body",
            crate::compaction::COMPACTION_SUMMARY_PREFIX
        ),
        ..Default::default()
    };
    let history = vec![
        LLMMessage {
            role: "system".into(),
            content: "system prompt".into(),
            ..Default::default()
        },
        summary,
        LLMMessage {
            role: "assistant".into(),
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
