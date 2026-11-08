use super::*;
use crate::rpc::types::StateReadRequest;
use crate::turn_loop::types::{LLMChatParams, LLMChatResponse};

fn spec() -> PipelineSpec {
    PipelineSpec {
        system_prompt: "sys".into(),
        model_name: "test-model".into(),
        providers: Vec::new(),
        native_llm: None,
        workspace_root: None,
        native_tools: false,
        extra_roots: Vec::new(),
        rust_self_contained: false,
        shell_path: None,
        policy_snapshot: None,
        github_token: None,
        github_base_url: None,
        subagent_timeout_ms: None,
        agent_tool_veto: None,
        tools_veto: None,
        todo_tool_veto: None,
        tower_worktree_root: None,
        tower_enabled: false,
        notify_user_enabled: true,
        tool_select: false,
        sandbox_mode: None,
        sandbox_policy: None,
        caller_agent_id: None,
        session_id: None,
        secondary_model: None,
        image_read_byte_budget: None,
        image_max_edge_px: None,
        model_capabilities: None,
        skill_dirs: Vec::new(),
        plugin_skill_dirs: Vec::new(),
        merge_all_available_skills: true,
        background: crate::storage::BackgroundLimits::default(),
    }
}

fn engine() -> ServerEngine {
    ServerEngine::new(
        spec(),
        Arc::new(EventHub::new()),
        Arc::new(SqliteSessionStore::in_memory().unwrap()),
    )
}

#[tokio::test]
async fn server_prompt_lists_plugin_skills_under_qualified_names() {
    let temp_dir = tempfile::tempdir().unwrap();
    let workspace = temp_dir.path().join("workspace");
    let plugin_root = temp_dir.path().join("plugin");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(plugin_root.join("lark-doc")).unwrap();
    std::fs::write(
        plugin_root.join("lark-doc").join("SKILL.md"),
        "---\nname: lark-doc\ndescription: from plugin a\n---\n",
    )
    .unwrap();

    let mut spec = spec();
    spec.workspace_root = Some(workspace.to_string_lossy().into_owned());
    let engine = ServerEngine::new(
        spec,
        Arc::new(EventHub::new()),
        Arc::new(SqliteSessionStore::in_memory().unwrap()),
    );
    let plugin_dirs = vec![crate::skills::PluginSkillDir {
        dir: plugin_root,
        plugin_id: "plugin-a".into(),
        instructions: None,
    }];
    engine.with_plugin_skill_roots(Arc::new(move || plugin_dirs.clone()));

    let session_spec = engine.session_spec("sess-plugin-skills").await;
    assert!(
        session_spec
            .system_prompt
            .contains("- plugin-a:lark-doc: from plugin a"),
        "the server prompt must advertise the exact qualified name the Skill tool resolves: {}",
        session_spec.system_prompt
    );
}

#[test]
fn tower_wake_runtime_is_shared_per_session_and_isolated_across_sessions() {
    let engine = engine();
    let server = Arc::new(crate::server::HttpServer::in_memory().unwrap());
    engine.install_tower_wake_driver(Arc::new(crate::server::HttpServerWakeDriver::new(server)));
    let dir = tempfile::tempdir().unwrap();
    let first =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let second =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());
    let other_session =
        Arc::new(crate::tools::NativeToolset::new(dir.path().to_str().unwrap(), None).unwrap());

    engine.bind_tower_wake(&first, "s1").unwrap();
    engine.bind_tower_wake(&second, "s1").unwrap();
    engine.bind_tower_wake(&other_session, "s2").unwrap();

    let first_runtime = first.tower_wake_runtime().unwrap();
    let second_runtime = second.tower_wake_runtime().unwrap();
    let other_runtime = other_session.tower_wake_runtime().unwrap();
    assert!(Arc::ptr_eq(&first_runtime, &second_runtime));
    assert!(!Arc::ptr_eq(&first_runtime, &other_runtime));
}

/// The status line's `contextTokens` used to be `content.len() / 4`, which
/// charges a quarter token per **byte** and therefore reads low on any
/// non-ASCII text — ~25% low on CJK (3 UTF-8 bytes per character), so the
/// gauge drifted low exactly in the sessions filling up. It also truncated:
/// 43 ASCII chars gave 10, not 11.
///
/// Both halves are pinned. ASCII moves by at most one token per message
/// (rounding, always upward). CJK is the substantive fix: 100 characters
/// must count as 100 tokens, where the byte shortcut said 75.
#[test]
fn context_tokens_count_cjk_per_character_and_leave_ascii_alone() {
    let est = crate::compaction::estimate_tokens;
    let ascii = "the quick brown fox jumps over the lazy dog"; // 43 chars
    assert_eq!(ascii.len(), 43);
    assert_eq!(
        est(ascii) as usize,
        ascii.len().div_ceil(4),
        "ASCII now rounds up instead of truncating: 43 chars -> 11, not 10"
    );
    assert!(
        (est(ascii) as usize).abs_diff(ascii.len() / 4) <= 1,
        "the ASCII difference is rounding only, never the 25% the CJK case had"
    );

    // 100 CJK chars = 300 UTF-8 bytes. Old: 300/4 = 75. New: 100.
    let cjk = "中".repeat(100);
    assert_eq!(cjk.len(), 300, "3 UTF-8 bytes per CJK char");
    assert_eq!(
        est(&cjk) as usize,
        100,
        "one token per CJK character, not one per 4 bytes"
    );
    assert_eq!(
        cjk.len() / 4,
        75,
        "the old shortcut under-reported by 25%, which is the bug"
    );

    // Mixed content: per-character totals, not a byte average. Note this is
    // NOT `est(cjk) + est(ascii)` — the ASCII chars are contiguous here, so
    // one `div_ceil` covers the whole run rather than one per fragment.
    let mixed = format!("{cjk}hello");
    assert_eq!(
        est(&mixed) as usize,
        100 + "hello".len().div_ceil(4),
        "CJK chars count one each; the ASCII tail rounds up as a single run"
    );
    assert!(
        (est(&mixed) as usize) > cjk.len() / 4 + "hello".len() / 4,
        "the mixed string must still exceed what the byte shortcut reported"
    );
}

/// The idle clock is the gate's input, so it has to be per session and has to
/// advance only when a turn actually produced output.
#[tokio::test]
async fn the_assistant_idle_clock_is_per_session_and_starts_empty() {
    let engine = engine();
    assert_eq!(
        engine.last_assistant_at_for("s1"),
        None,
        "a session that never produced output has no idle age, so micro \
             compaction cannot trigger (v2's `cacheAgeMs === null`)"
    );
    assert_eq!(engine.last_assistant_at_for("s2"), None);

    engine.stamp_last_assistant_at("s1");
    let stamped = engine.last_assistant_at_for("s1").expect("s1 was stamped");
    assert!(
        stamped > 0,
        "the stamp is a wall-clock millis value, got {stamped}"
    );
    assert_eq!(
        engine.last_assistant_at_for("s2"),
        None,
        "stamping one session must not make another look idle"
    );
}

/// The window lookup backs micro compaction's fullness gate. An unknown
/// model must read as "no window" rather than as zero — the gate treats an
/// absent window as full, and a zero would invert that.
#[tokio::test]
async fn the_context_window_is_unknown_for_a_model_with_no_size() {
    let engine = engine();
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(
        crate::config::KimiConfig::default(),
    ))));
    assert_eq!(
        engine.model_context_window("s1").await,
        None,
        "no alias for the model means an unknown window, not a zero one"
    );
}

/// The `[experimental].micro_compaction` flag gates the wiring: off/unset
/// yields no config (the transform never runs), `true`/non-`false` strings
/// enable it.
#[tokio::test]
async fn micro_compaction_flag_reads_the_experimental_map() {
    let with_flag = |value: Option<crate::config::ExperimentalValue>| {
        let engine = engine();
        let mut config = crate::config::KimiConfig::default();
        if let Some(value) = value {
            config
                .experimental
                .insert("micro_compaction".to_string(), value);
        }
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        engine
    };

    assert!(
        with_flag(None).micro_compaction_config().await.is_none(),
        "unset flag stays off"
    );
    assert!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(false)))
            .micro_compaction_config()
            .await
            .is_none(),
    );
    assert!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "false".into()
        )))
        .micro_compaction_config()
        .await
        .is_none(),
    );
    assert!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(true)))
            .micro_compaction_config()
            .await
            .is_some(),
    );
    assert!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "true".into()
        )))
        .micro_compaction_config()
        .await
        .is_some(),
    );
}

/// The `[experimental].thinking_repeat_guard` flag drives the guard the
/// same way: unset yields no flag (the caller defaults it to on),
/// `true`/non-`false` strings enable it, `false` disables it.
#[tokio::test]
async fn thinking_guard_flag_reads_the_experimental_map() {
    let with_flag = |value: Option<crate::config::ExperimentalValue>| {
        let engine = engine();
        let mut config = crate::config::KimiConfig::default();
        if let Some(value) = value {
            config.experimental.insert(
                crate::llm::thinking_guard::GUARD_CONFIG_KEY.to_string(),
                value,
            );
        }
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        engine
    };

    assert_eq!(
        with_flag(None).thinking_guard_config_flag().await,
        None,
        "unset flag is not a value"
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(false)))
            .thinking_guard_config_flag()
            .await,
        Some(false),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "false".into()
        )))
        .thinking_guard_config_flag()
        .await,
        Some(false),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(true)))
            .thinking_guard_config_flag()
            .await,
        Some(true),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "true".into()
        )))
        .thinking_guard_config_flag()
        .await,
        Some(true),
    );
    assert_eq!(
        engine().thinking_guard_config_flag().await,
        None,
        "no config source leaves the flag unresolved"
    );
}

/// The `[experimental].memory_filing` flag gates the background filing
/// pass the same way: unset/off yields no flag, `true`/non-`false`
/// strings enable it. The env override is resolved on top of this, so the
/// config path is asserted on its own.
#[tokio::test]
async fn memory_filing_flag_reads_the_experimental_map() {
    let with_flag = |value: Option<crate::config::ExperimentalValue>| {
        let engine = engine();
        let mut config = crate::config::KimiConfig::default();
        if let Some(value) = value {
            config
                .experimental
                .insert(memory_filing::FILING_CONFIG_KEY.to_string(), value);
        }
        engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
        engine
    };

    assert_eq!(
        with_flag(None).memory_filing_config_flag().await,
        None,
        "unset flag is not a value"
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(false)))
            .memory_filing_config_flag()
            .await,
        Some(false),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "false".into()
        )))
        .memory_filing_config_flag()
        .await,
        Some(false),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::Bool(true)))
            .memory_filing_config_flag()
            .await,
        Some(true),
    );
    assert_eq!(
        with_flag(Some(crate::config::ExperimentalValue::String(
            "true".into()
        )))
        .memory_filing_config_flag()
        .await,
        Some(true),
    );
    assert_eq!(
        engine().memory_filing_config_flag().await,
        None,
        "no config source leaves the flag unresolved"
    );
}

#[test]
fn session_profile_overrides_spec_for_the_turn() {
    let mut spec = spec();
    spec.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        model: "base".into(),
        reasoning_effort: Some("low".into()),
        ..Default::default()
    });
    let profile = serde_json::json!({
        "model": "alias-2",
        "thinking": "high",
        "disabled_tools": ["Bash", "Write"],
    });
    apply_session_overrides(&mut spec, &profile);
    assert_eq!(spec.model_name, "alias-2");
    assert_eq!(spec.native_llm.as_ref().unwrap().model, "alias-2");
    assert_eq!(
        spec.native_llm
            .as_ref()
            .unwrap()
            .reasoning_effort
            .as_deref(),
        Some("high")
    );
    let filter = spec
        .policy_snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.tools_filter.as_ref())
        .expect("tools filter");
    assert!(filter.disabled.contains(&"Bash".to_string()));
    assert!(filter.disabled.contains(&"Write".to_string()));
}

#[test]
fn empty_session_profile_leaves_spec_untouched() {
    let mut spec = spec();
    spec.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        model: "base".into(),
        ..Default::default()
    });
    apply_session_overrides(&mut spec, &serde_json::json!({}));
    assert_eq!(spec.model_name, "test-model");
    assert_eq!(spec.native_llm.as_ref().unwrap().model, "base");
    assert!(spec.policy_snapshot.is_none());
}

#[tokio::test]
async fn session_model_resolves_cross_provider_from_config() {
    let engine = engine();
    let config: crate::config::KimiConfig = r#"
default_model = "alias-2"

[providers.acme]
type = "openai"
api_key = "k"
base_url = "https://api.example.test/v1"

[models.alias-2]
provider = "acme"
model = "gpt-x"
"#
    .parse()
    .expect("parse config");
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));

    let native = engine
        .resolved_native_llm("alias-2")
        .await
        .expect("alias resolves to its provider");
    assert_eq!(native.base_url, "https://api.example.test/v1");
    assert_eq!(native.model, "gpt-x");
    assert_eq!(native.api_key, "k");
}

/// An env-bound provider keeps its credential channel through the server
/// engine's resolution: the transport reads the named variable at request
/// time, so dropping it here would send every request with an empty key.
#[tokio::test]
async fn session_model_keeps_the_env_bound_credential_channel() {
    let engine = engine();
    let config: crate::config::KimiConfig = r#"
default_model = "alias-2"

[providers.acme]
type = "openai"
api_key_env = "KIMI_TEST_PROVIDER_API_KEY"
base_url = "https://api.example.test/v1"

[models.alias-2]
provider = "acme"
model = "gpt-x"
"#
    .parse()
    .expect("parse config");
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));

    let native = engine
        .resolved_native_llm("alias-2")
        .await
        .expect("alias resolves to its provider");
    assert_eq!(native.api_key, "");
    assert_eq!(
        native.api_key_env.as_deref(),
        Some("KIMI_TEST_PROVIDER_API_KEY")
    );
}

/// `llm_for_model` resolves the named alias through the same chain a turn
/// uses, and reports an unresolvable one as `None` instead of a transport
/// error — the debug surface's ping turns that into an honest failure.
#[tokio::test]
async fn llm_for_model_resolves_a_named_alias_and_refuses_an_unknown_one() {
    let engine = engine();
    let config: crate::config::KimiConfig = r#"
[providers.acme]
type = "openai"
api_key = "k"
base_url = "https://api.example.test/v1"

[models.alias-2]
provider = "acme"
model = "gpt-x"
"#
    .parse()
    .expect("parse config");
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));

    let llm = engine
        .llm_for_model("alias-2")
        .await
        .expect("alias resolves to its provider");
    assert_eq!(llm.model_name(), "gpt-x");
    assert!(engine.llm_for_model("no-such-alias").await.is_none());
}

struct ScriptedLlm;

impl LLM for ScriptedLlm {
    fn system_prompt(&self) -> &str {
        "sys"
    }
    fn model_name(&self) -> &str {
        "scripted"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            Ok(LLMChatResponse {
                content: "done".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: TokenUsage::default(),
                timing: None,
            })
        })
    }
}

#[tokio::test]
async fn the_absent_host_refuses_instead_of_waiting() {
    let host = ServerHost::standalone();

    let error = host
        .llm_chat(LlmChatRequest {
            system_prompt: "sys".into(),
            model_name: "m".into(),
            messages: Vec::new(),
            tools: Vec::new(),
            request_id: None,
        })
        .await
        .unwrap_err();
    assert!(error.contains("no LLM host"), "{error}");

    let error = host
        .execute_tool(ToolExecuteRequest {
            agent_id: crate::callbacks::MAIN_AGENT_ID.to_string(),
            turn_id: "t".into(),
            tool_call_id: "c".into(),
            tool_name: "WebSearch".into(),
            arguments: serde_json::json!({}),
        })
        .await
        .unwrap_err();
    assert!(error.contains("WebSearch"), "{error}");

    let verdict = host
        .check_permission(PermissionCheckRequest {
            tool_name: "Bash".into(),
            session_approval_rule: None,
            tool_call_id: "c".into(),
            turn_id: "turn-1".into(),
            arguments: serde_json::json!({}),
            reason: None,
        })
        .await
        .unwrap();
    assert_eq!(verdict.decision, "deny");

    // Legs left on the trait's defaults must also answer, not hang.
    assert!(host.list_tools().await.is_err());
    assert!(host.goal().await.is_err());
    assert!(
        host.state_read(StateReadRequest {
            domain: "plan".into(),
            key: "plan".into(),
            turn_id: String::new(),
            tool_call_id: String::new(),
        })
        .await
        .is_err()
    );
}

#[tokio::test]
async fn a_turn_without_any_configured_model_is_refused_up_front() {
    let engine = engine();
    let error = engine
        .run_turn("sess-1", 1, Vec::new(), "hello")
        .await
        .expect_err("no providers and no native_llm must not run");

    match error {
        EngineError::NoModel(message) => {
            assert!(message.contains("rustSelfContained"), "{message}")
        }
        other => panic!("expected NoModel, got {other:?}"),
    }
}

#[tokio::test]
async fn a_turn_on_a_foreign_held_session_is_refused_as_occupied() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-occupied-engine", Some("occupied test"))
        .unwrap();
    // A second service on the same directory stands in for another process:
    // its holder file carries this pid (alive on every platform) with a
    // token this process never claimed, exactly what a foreign hold writes.
    let foreign = crate::session::lock::SessionLockService::new(
        crate::session::lock::SESSION_LOCKS.lock_dir().to_path_buf(),
    );
    foreign.hold("sess-occupied-engine").unwrap();

    let error = engine
        .run_turn_on(&ScriptedLlm, "sess-occupied-engine", 1, Vec::new(), "hello")
        .await
        .expect_err("a foreign-held session must refuse the turn");
    match error {
        EngineError::Occupied(message) => assert_eq!(
            message,
            "This session is in use by another Kimi Code process. Stop the session in that process or exit it, then try again."
        ),
        other => panic!("expected Occupied, got {other:?}"),
    }
    // The refused turn left no trace: no busy marker, no history.
    assert!(!engine.is_busy("sess-occupied-engine"));

    // Once the foreign holder lets go, the next turn claims the session.
    foreign.release("sess-occupied-engine");
    engine
        .run_turn_on(&ScriptedLlm, "sess-occupied-engine", 1, Vec::new(), "hello")
        .await
        .expect("a freed session runs");
}

#[tokio::test]
async fn a_turn_runs_persists_and_reports_its_transport() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-1", Some("engine test"))
        .unwrap();

    let report = engine
        .run_turn_on(&ScriptedLlm, "sess-1", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");

    assert_eq!(report.steps, 1);
    assert_eq!(report.stop_reason, "EndTurn");
    // Counting-wrapper figures are zero on this path: no pipeline was built,
    // so nothing was counted. Asserting it keeps the seam honest.
    assert_eq!(report.events_emitted, 0);
    assert_eq!(report.native_tool_calls, 0);

    let history = engine.store().load_session_history("sess-1").unwrap();
    let contents: Vec<&str> = history.iter().map(|m| m.content.as_str()).collect();
    assert!(
        contents.contains(&"hello"),
        "prompt not persisted: {contents:?}"
    );
    assert!(
        contents.contains(&"done"),
        "reply not persisted: {contents:?}"
    );
    // The loop owns its own message list (system prompt plus an injected
    // context turn); the store must hold only the non-system transcript.
    assert!(
        !history.iter().any(|m| m.role == "system"),
        "system row persisted as conversation: {contents:?}"
    );
}

#[tokio::test]
async fn a_second_turn_does_not_re_write_the_carried_history() {
    let engine = engine();
    engine.store().create_session("sess-3", None).unwrap();

    engine
        .run_turn_on(&ScriptedLlm, "sess-3", 1, Vec::new(), "first")
        .await
        .unwrap();
    let after_first = engine.store().load_session_history("sess-3").unwrap();
    let first_prompts = after_first.iter().filter(|m| m.content == "first").count();

    engine
        .run_turn_on(&ScriptedLlm, "sess-3", 2, after_first, "second")
        .await
        .unwrap();
    let history = engine.store().load_session_history("sess-3").unwrap();

    assert_eq!(first_prompts, 1, "turn 1 duplicated its own prompt");
    assert_eq!(
        history.iter().filter(|m| m.content == "first").count(),
        1,
        "turn 2 re-persisted the carried history: {:?}",
        history.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert!(history.iter().any(|m| m.content == "second"));
}

#[tokio::test]
async fn history_before_the_prompt_is_carried_into_the_turn() {
    let engine = engine();
    engine.store().create_session("sess-2", None).unwrap();

    let history = vec![LLMMessage {
        role: "user".into(),
        content: "earlier question".into(),
        ..Default::default()
    }];
    let report = engine
        .run_turn_on(&ScriptedLlm, "sess-2", 2, history, "follow-up")
        .await
        .expect("turn completes");

    // The prompt is appended after the history, so the loop sees both.
    assert_eq!(report.steps, 1);
}

#[tokio::test]
async fn a_turn_publishes_work_changed_busy_then_idle() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-wc", Some("work changed test"))
        .unwrap();
    let mut sub = engine.hub().attach();

    let report = engine
        .run_turn_on(&ScriptedLlm, "sess-wc", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn");
    assert_eq!(report.stop_reason, "EndTurn");

    // The turn boundary now publishes its facts in a fixed order — the
    // work_changed busy flip, the deduped status snapshot and the
    // activity phase at start; the typed turn.ended, the work_changed
    // flip back and the phase again at end. Every recv is bounded
    // so a regression fails instead of hanging the suite.
    async fn next_event(
        sub: &mut crate::server::hub::WsSubscription,
    ) -> std::sync::Arc<crate::server::hub::SequencedEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(5), sub.recv())
            .await
            .expect("event within 5s")
            .expect("hub open")
    }
    let events: Vec<std::sync::Arc<crate::server::hub::SequencedEvent>> = {
        let mut collected = Vec::new();
        for _ in 0..9 {
            collected.push(next_event(&mut sub).await);
        }
        collected
    };
    let sequence: Vec<String> = events
        .iter()
        .map(|e| e.event.event_type().to_string())
        .collect();
    assert_eq!(
        sequence,
        vec![
            "event.session.work_changed",  // busy=true
            "agent.status.updated",        // snapshot (first for the session)
            "agent.status.updated",        // phase: running
            "event.message.created",       // the user prompt
            "turn.ended",                  // the turn closes, before idle
            "event.session.work_changed",  // busy=false
            "agent.status.updated",        // snapshot (context grew)
            "event.session.usage_updated", // live token accounting
            "agent.status.updated",        // phase: ended
        ]
    );

    let crate::events::EngineEvent::SessionWorkChanged {
        busy,
        main_turn_active,
        pending_interaction,
        last_turn_reason,
    } = &events[0].event
    else {
        panic!("expected work_changed, got {:?}", events[0].event);
    };
    assert!(*busy);
    assert!(*main_turn_active);
    assert_eq!(pending_interaction, "none");
    assert!(last_turn_reason.is_none());

    let crate::events::EngineEvent::Custom(start_phase) = &events[2].event else {
        panic!("expected the running phase, got {:?}", events[2].event);
    };
    assert_eq!(start_phase["phase"]["kind"], "running");

    let crate::events::EngineEvent::Custom(end_phase) = &events[8].event else {
        panic!("expected the ended phase, got {:?}", events[8].event);
    };
    assert_eq!(end_phase["phase"]["kind"], "ended");
    assert_eq!(end_phase["phase"]["reason"], "completed");

    // The turn closes on the typed event, scoped by its turn number and
    // carrying the loop's end reason — the v2 turn-lifecycle contract.
    let crate::events::EngineEvent::TurnEnded {
        turn_id, reason, ..
    } = &events[4].event
    else {
        panic!("expected turn.ended, got {:?}", events[4].event);
    };
    assert_eq!(turn_id, "turn-1");
    assert_eq!(reason, "completed");

    let crate::events::EngineEvent::SessionWorkChanged {
        busy,
        last_turn_reason,
        ..
    } = &events[5].event
    else {
        panic!("expected work_changed, got {:?}", events[5].event);
    };
    assert!(!busy);
    assert_eq!(last_turn_reason.as_deref(), Some("completed"));

    // The mirror also lands on the session row. The live event asserted
    // above is not enough: it is gone once the turn is over, so a client
    // that attaches later — or after a restart — reads the outcome from
    // here. Replacing the persisted value with `None` left this whole file
    // green, because only the event was ever asserted.
    assert_eq!(
        engine.store().last_turn_reason("sess-wc").as_deref(),
        Some("completed"),
        "the turn outcome must be persisted, not only published"
    );
}

#[tokio::test]
async fn status_updated_folds_agent_config_and_context_then_dedups() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-status", Some("status test"))
        .unwrap();
    engine
        .store
        .put_state(
            "agent_config",
            "sess-status",
            &serde_json::json!({
                "model": "k3-test",
                "thinking": "high",
                "permission_mode": "manual",
                "plan_mode": true,
            }),
        )
        .unwrap();
    // Four content bytes → the same `len() / 4` estimate the wire session
    // reports, so the assertion pins the exact budget.
    engine
        .store
        .save_turn(
            "sess-status",
            "turn-status",
            1,
            &[LLMMessage::user("abcd")],
            None,
            None,
        )
        .unwrap();
    let mut sub = engine.hub().attach();

    engine.publish_status_updated("sess-status").await;
    let event = sub.recv().await.unwrap();
    assert_eq!(&*event.session_id, "sess-status");
    assert_eq!(event.event.event_type(), "agent.status.updated");
    let crate::events::EngineEvent::Custom(payload) = &event.event else {
        panic!("expected a Custom status payload");
    };
    assert_eq!(payload["model"], "k3-test");
    assert_eq!(payload["thinkingEffort"], "high");
    assert_eq!(payload["permission"], "manual");
    assert_eq!(payload["planMode"], true);
    assert_eq!(payload["contextTokens"], 1);

    // An unchanged state re-publish is dropped by the snapshot dedup.
    engine.publish_status_updated("sess-status").await;
    let next = tokio::time::timeout(std::time::Duration::from_millis(50), sub.recv()).await;
    assert!(next.is_err(), "dedup must swallow the identical payload");
}

#[test]
fn work_turn_reason_maps_the_loop_vocabulary() {
    use crate::turn_loop::types::LoopTurnStopReason;
    assert_eq!(work_turn_reason(&LoopTurnStopReason::EndTurn), "completed");
    assert_eq!(work_turn_reason(&LoopTurnStopReason::MaxSteps), "failed");
    assert_eq!(work_turn_reason(&LoopTurnStopReason::Filtered), "failed");
    assert_eq!(work_turn_reason(&LoopTurnStopReason::Aborted), "cancelled");
}

/// v2 `buildContextCompactionShape` (`compactionHandoff.ts:107-111`): after a
/// compaction the summary message's size is the summarization request's
/// measured output, not an estimate of its text. Without that substitution the
/// context gauge keeps guessing the largest fixed message in the new history.
#[test]
fn the_rebase_substitutes_the_measured_summary_output() {
    use crate::turn_loop::types::LLMMessage;
    let summary = LLMMessage {
        role: "user".into(),
        content: format!(
            "{} the summary body",
            crate::compaction::COMPACTION_SUMMARY_PREFIX
        ),
        ..Default::default()
    };
    let messages = vec![
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
    let estimate = crate::session::token_counting::estimate_messages(&messages);
    let summary_estimate = crate::session::token_counting::estimate_message(&messages[1]);

    assert_eq!(
        rebased_tokens(&messages, None),
        estimate,
        "without a measurement the estimate stands"
    );
    assert_eq!(
        rebased_tokens(&messages, Some(0)),
        estimate - summary_estimate,
        "a measured zero replaces the estimate entirely"
    );
    assert_eq!(
        rebased_tokens(&messages, Some(7)),
        estimate - summary_estimate + 7,
        "the measured count is what the summary costs"
    );
    assert_eq!(
        rebased_tokens(&messages[..1], Some(7)),
        crate::session::token_counting::estimate_messages(&messages[..1]),
        "a history without the summary message has nothing to substitute"
    );
}

/// v2 `TurnEnded` filter: the interruption reminder arms only when the turn
/// was cancelled **by the user** (`interruptReason === 'user_cancelled'`). A
/// teardown flips the same cancel flag and lands the same `cancelled` reason,
/// but telling the model "the user interrupted" after a shutdown is false.
#[test]
fn only_a_user_cancel_arms_the_interruption_reminder() {
    let engine = engine();

    engine.publish_turn_ended("s", 1, "cancelled");
    assert!(
        !engine.take_last_turn_aborted("s"),
        "a cancelled turn nobody asked for is a teardown, not a user interrupt"
    );

    engine.publish_turn_ended("s", 2, "completed");
    assert!(!engine.take_last_turn_aborted("s"));

    let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
    engine
        .active_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert("s".into(), flag.clone());
    assert!(engine.cancel_turn("s"), "the running turn is cancellable");
    assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
    engine.publish_turn_ended("s", 3, "cancelled");
    assert!(
        engine.take_last_turn_aborted("s"),
        "the user's stop arms the reminder for the next turn"
    );
    assert!(
        !engine.take_last_turn_aborted("s"),
        "the trigger is read-and-clear"
    );
}

#[test]
fn engine_with_mcp_manager_wires_and_exposes_manager() {
    let engine = engine();
    assert!(engine.mcp_manager().is_none());

    let mcp = Arc::new(McpManager::new());
    let engine = engine.with_mcp_manager(mcp);
    assert!(engine.mcp_manager().is_some());
}

#[test]
fn enqueue_steer_refuses_and_leaves_the_signal_alone_when_no_turn_runs() {
    // Nothing to steer without an active turn, and a refused steer must not
    // fire the signal for whatever turn comes next.
    let engine = engine();
    let signal = crate::subagent::types::ParentCancel::new();
    *engine
        .steer_slot("sess-steer")
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(signal.clone());

    assert!(!engine.enqueue_steer("sess-steer", LLMMessage::user("steer-me")));
    assert_eq!(engine.queued_steer_count("sess-steer"), 0);
    assert!(!signal.triggered());
}

#[test]
fn enqueue_steer_queues_and_fires_the_turn_signal() {
    let engine = engine();
    let signal = crate::subagent::types::ParentCancel::new();
    *engine
        .steer_slot("sess-steer")
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(signal.clone());
    engine
        .active_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            "sess-steer".to_string(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );

    assert!(engine.enqueue_steer("sess-steer", LLMMessage::user("steer-me")));
    assert_eq!(engine.queued_steer_count("sess-steer"), 1);
    assert!(
        signal.triggered(),
        "a steer joining the running turn must fire its signal"
    );
}

/// A steer the cancelled turn never drained is not lost: the queue
/// survives the turn boundary and the session's next turn drains it at
/// its first step head (v2's unconsumed-steer semantics; the session
/// path's session-scoped queue behaves the same). The cancel check runs
/// at the step top, ahead of `drain_steers`, so this is exactly the
/// window where the message used to vanish with the turn.
#[test]
fn an_undrained_steer_survives_the_turn_boundary_for_the_next_turn() {
    let engine = engine();
    let active = |session: &str| {
        engine
            .active_turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session.to_string(),
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            );
    };
    active("sess-steer-survive");
    assert!(engine.enqueue_steer("sess-steer-survive", LLMMessage::user("stop")));

    // The turn ends: the guard drops the active-turn entry, then the
    // steer bookkeeping (cancel at the step top, before the drain).
    engine
        .active_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove("sess-steer-survive");
    engine.release_turn_steer_state("sess-steer-survive");

    assert_eq!(
        engine.queued_steer_count("sess-steer-survive"),
        1,
        "the undrained steer must survive the turn boundary"
    );
    // The next turn's callbacks wrap this very queue
    // (`SteerQueueCallbacks::new(.., self.steer_queue(session))`), so the
    // message reaches that turn's first step head.
    let queue = engine.steer_queue("sess-steer-survive");
    assert_eq!(queue.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);
    // A surviving queue does not make a nonexistent turn steerable: with
    // no active turn the route still refuses and re-queues the prompt.
    assert!(!engine.enqueue_steer("sess-steer-survive", LLMMessage::user("late")));
}

/// An empty queue is dropped at the turn boundary, so the map does not
/// grow one entry per session over a server's lifetime.
#[test]
fn an_empty_steer_queue_is_dropped_at_the_turn_boundary() {
    let engine = engine();
    engine
        .active_turns
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            "sess-steer-empty".to_string(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
    // A turn that drained everything (or never got a steer) leaves nothing.
    engine.steer_queue("sess-steer-empty");
    engine.release_turn_steer_state("sess-steer-empty");
    assert!(
        engine
            .steer_queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get("sess-steer-empty")
            .is_none(),
        "an empty queue must not outlive its turn"
    );
}

/// An LLM that reports real counters, so the engine's usage fold can be read
/// back off the status payload.
struct UsageLlm;

impl LLM for UsageLlm {
    fn system_prompt(&self) -> &str {
        "sys"
    }
    fn model_name(&self) -> &str {
        "usage-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        Box::pin(async {
            Ok(LLMChatResponse {
                content: "done".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    input_cache_read: 3,
                    input_cache_creation: 2,
                },
                timing: None,
            })
        })
    }
}

/// A finished turn records its usage under the LLM's model name, and the
/// status payload carries `usage` in the camelCase wire shape.
#[tokio::test]
async fn a_finished_turn_records_usage_per_model_and_the_status_carries_it() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-usage", Some("usage test"))
        .unwrap();

    engine
        .run_turn_on(&UsageLlm, "sess-usage", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");

    let payload = engine.status_payload("sess-usage");
    let usage = &payload["usage"];
    assert_eq!(usage["byModel"]["usage-model"]["inputOther"], 10);
    assert_eq!(usage["byModel"]["usage-model"]["output"], 5);
    assert_eq!(usage["byModel"]["usage-model"]["inputCacheRead"], 3);
    assert_eq!(usage["byModel"]["usage-model"]["inputCacheCreation"], 2);
    assert_eq!(usage["total"]["inputOther"], 10);
    assert_eq!(usage["currentTurn"]["output"], 5);
}

/// The configured model window reaches the turn loop (compaction and the
/// LLM completion budget) and comes back on the report for hosts that
/// render the context gauge.
#[tokio::test]
async fn a_turn_carries_the_configured_context_window() {
    let engine = engine();
    let mut config = crate::config::KimiConfig::default();
    config.models.insert(
        "test-model".into(),
        crate::config::ModelAliasConfig {
            max_context_size: Some(12_345),
            ..Default::default()
        },
    );
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
    engine.store().create_session("sess-window", None).unwrap();

    let report = engine
        .run_turn_on(&ScriptedLlm, "sess-window", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");
    assert_eq!(report.context_window, Some(12_345));
}

/// v2 scopes todo / plan / goal / task / turn to the agent, not the
/// workspace: two sessions of one checkout must not see each other's todo.
/// A workspace value written before the scope change remains readable as a
/// legacy fallback for a session that has not written the domain yet.
#[test]
fn session_state_is_isolated_between_sessions_of_one_workspace() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let workspace = engine
        .store()
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    for sid in ["sess-a", "sess-b"] {
        engine
            .store()
            .create_session_with_workspace(sid, None, Some(&workspace.id))
            .unwrap();
    }
    crate::storage::StateStore::for_session(dir.path(), "sess-a")
        .unwrap()
        .write_domain(
            "todo",
            &serde_json::json!([{ "id": "T1", "title": "only-a", "status": "pending" }]),
        )
        .unwrap();

    assert_eq!(engine.session_todos("sess-a").len(), 1);
    assert!(
        engine.session_todos("sess-b").is_empty(),
        "another session's todo must not leak through the workspace"
    );

    // Legacy (pre-scope) workspace data stays readable until the session
    // writes its own value, then the session store wins.
    crate::storage::StateStore::for_workspace(dir.path())
        .unwrap()
        .write_domain(
            "todo",
            &serde_json::json!([{ "id": "T9", "title": "legacy", "status": "pending" }]),
        )
        .unwrap();
    let legacy = engine.session_todos("sess-b");
    assert_eq!(legacy.len(), 1);
    assert_eq!(legacy[0].title, "legacy");
}

/// The callbacks a session's turns run with own that session's state store,
/// so a turn-head checkpoint snapshots exactly this session — the anchor a
/// later undo restores (and never another session's stack).
#[tokio::test]
async fn a_session_checkpoint_snapshots_only_that_sessions_state() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let workspace = engine
        .store()
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    for sid in ["sess-cp-a", "sess-cp-b"] {
        engine
            .store()
            .create_session_with_workspace(sid, None, Some(&workspace.id))
            .unwrap();
    }
    let session_store = crate::storage::StateStore::for_session(dir.path(), "sess-cp-a").unwrap();
    session_store
        .write_domain(
            "todo",
            &serde_json::json!([{ "id": "T1", "title": "work", "status": "pending" }]),
        )
        .unwrap();

    let callbacks = engine.session_callbacks("sess-cp-a", dir.path());
    callbacks
        .checkpoint_state()
        .await
        .expect("the session's callbacks checkpoint its state store");

    assert_eq!(session_store.checkpoint_depth(), 1);
    assert_eq!(
        crate::storage::StateStore::for_session(dir.path(), "sess-cp-b")
            .unwrap()
            .checkpoint_depth(),
        0,
        "another session's checkpoint stack must stay untouched"
    );
}

/// v2 folds a finished turn into an active goal (`incrementGoalTurn` +
/// `accountTokenUsage`) and launches a continuation while the goal is still
/// active; both halves live in the engine on this entry point because no host
/// loop re-prompts a server or ACP turn.
#[tokio::test]
async fn a_finished_turn_folds_into_the_sessions_goal_and_offers_its_continuation() {
    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let workspace = engine
        .store()
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    engine
        .store()
        .create_session_with_workspace("sess-goal", None, Some(&workspace.id))
        .unwrap();
    let store = crate::storage::StateStore::for_session(dir.path(), "sess-goal").unwrap();
    let outcome = store
        .apply_write(
            "goal",
            &serde_json::json!({ "action": "create", "objective": "ship the thing" }),
        )
        .unwrap();
    store.write_domain("goal", &outcome.stored).unwrap();

    let continuation = engine
        .pending_goal_continuation("sess-goal", false)
        .await
        .expect("an active goal owes its continuation");
    assert!(
        continuation.starts_with("Continue working toward the active goal."),
        "{continuation}"
    );
    assert!(
        engine
            .pending_goal_continuation("sess-goal", true)
            .await
            .is_some_and(|prompt| prompt.contains("reached the per-turn step limit")),
        "a step-capped continuation carries the v2 step-cap prefix"
    );

    engine
        .run_turn_on(&UsageLlm, "sess-goal", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");

    let goal = store.goal_context().expect("the goal is still stored");
    assert_eq!(goal.turns_used, 1, "the turn is folded into the goal");
    assert_eq!(goal.tokens_used, 5, "the turn's output tokens are folded");
    assert!(
        engine
            .pending_goal_continuation("sess-goal", false)
            .await
            .is_some(),
        "the goal is still active, so it still owes turns"
    );

    let outcome = store
        .apply_write(
            "goal",
            &serde_json::json!({ "action": "update", "status": "complete" }),
        )
        .unwrap();
    store.write_domain("goal", &outcome.stored).unwrap();
    assert!(
        engine
            .pending_goal_continuation("sess-goal", false)
            .await
            .is_none(),
        "a completed goal owes no continuation"
    );
}

/// The status gauge is measured + estimated (v2 `statusSize`): the turn's
/// last request anchors the context it served, so the gauge reports the
/// provider's total rather than a small content estimate.
#[tokio::test]
async fn a_finished_turn_anchors_the_context_gauge_on_its_last_request() {
    let engine = engine();
    engine.store().create_session("sess-count", None).unwrap();

    engine
        .run_turn_on(&UsageLlm, "sess-count", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");

    // UsageLlm reports input 10, cache read 3, cache creation 2, output 5 —
    // v2's measured total is their sum.
    let measured = 20;
    assert_eq!(engine.session_context_tokens("sess-count"), measured);
    assert_eq!(
        engine.status_payload("sess-count")["contextTokens"],
        measured,
        "the gauge carries the measured total"
    );
}

/// A transcript cut re-baselines the gauge: anchors that measured the removed
/// messages no longer apply (v2 `recordTruncation` on undo).
#[tokio::test]
async fn truncating_the_transcript_rebases_the_context_gauge() {
    let engine = engine();
    engine.store().create_session("sess-cut", None).unwrap();
    engine
        .run_turn_on(&UsageLlm, "sess-cut", 1, Vec::new(), "hello")
        .await
        .expect("scripted turn completes");
    assert_eq!(engine.session_context_tokens("sess-cut"), 20);

    let undone = engine.store().undo_turns("sess-cut", 1).expect("undo");
    assert_eq!(undone, 1);
    engine.record_context_truncation("sess-cut");
    assert_eq!(
        engine.session_context_tokens("sess-cut"),
        0,
        "no messages remain, so no tokens remain"
    );
    assert_eq!(engine.status_payload("sess-cut")["contextTokens"], 0);
}

/// The todo list the ACP `plan` notification projects: read from the same
/// workspace state domain the todo tools write, empty for an unknown
/// session.
#[test]
fn session_todos_reads_the_workspace_todo_domain() {
    use crate::tools::todo_item::TodoStatus;

    let engine = engine();
    let dir = tempfile::tempdir().unwrap();
    let workspace = engine
        .store()
        .create_workspace(dir.path().to_str().unwrap(), None)
        .unwrap();
    engine
        .store()
        .create_session_with_workspace("sess-todos", None, Some(&workspace.id))
        .unwrap();
    let state = crate::storage::StateStore::for_workspace(dir.path()).unwrap();
    state
        .write_domain(
            "todo",
            &serde_json::json!([
                { "id": "T1", "title": "Read", "status": "in_progress", "progress": 40 },
                { "id": "T2", "title": "Write", "status": "done" }
            ]),
        )
        .unwrap();

    let todos = engine.session_todos("sess-todos");
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].title, "Read");
    assert_eq!(todos[0].status, TodoStatus::InProgress);
    assert_eq!(todos[1].title, "Write");
    assert_eq!(todos[1].status, TodoStatus::Done);
    assert!(engine.session_todos("missing").is_empty());
}

#[test]
fn session_thinking_maps_to_the_wire_dialect_of_the_protocol() {
    let mut anthropic = spec();
    anthropic.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        protocol: "anthropic".into(),
        model: "base".into(),
        ..Default::default()
    });
    apply_session_overrides(&mut anthropic, &serde_json::json!({ "thinking": "high" }));
    let native = anthropic.native_llm.as_ref().unwrap();
    assert_eq!(native.thinking_budget, Some(32_000));
    assert_eq!(native.reasoning_effort, None);

    let mut openai = spec();
    openai.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        protocol: "openai".into(),
        model: "base".into(),
        thinking_budget: Some(1024),
        ..Default::default()
    });
    apply_session_overrides(&mut openai, &serde_json::json!({ "thinking": "medium" }));
    let native = openai.native_llm.as_ref().unwrap();
    assert_eq!(native.reasoning_effort.as_deref(), Some("medium"));
    assert_eq!(native.thinking_budget, None);

    // Kimi's anthropic wire takes the effort itself, and off must be
    // explicit so the transport can send its protocol-level disable.
    let mut kimi = spec();
    kimi.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        protocol: "anthropic".into(),
        provider_type: Some("kimi".into()),
        model: "base".into(),
        thinking_keep: Some("all".into()),
        ..Default::default()
    });
    apply_session_overrides(&mut kimi, &serde_json::json!({ "thinking": "high" }));
    let native = kimi.native_llm.as_ref().unwrap();
    assert_eq!(native.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(native.thinking_budget, None);
    assert_eq!(native.thinking_keep.as_deref(), Some("all"));

    apply_session_overrides(&mut kimi, &serde_json::json!({ "thinking": "off" }));
    let native = kimi.native_llm.as_ref().unwrap();
    assert_eq!(native.reasoning_effort.as_deref(), Some("off"));
    assert_eq!(native.thinking_keep, None);
}

#[test]
fn session_thinking_off_clears_the_budget_and_the_keep() {
    let mut spec = spec();
    spec.native_llm = Some(crate::rpc::types::NativeLlmConfig {
        protocol: "anthropic".into(),
        model: "base".into(),
        thinking_budget: Some(32_000),
        thinking_keep: Some("all".into()),
        ..Default::default()
    });
    apply_session_overrides(&mut spec, &serde_json::json!({ "thinking": "off" }));
    let native = spec.native_llm.as_ref().unwrap();
    assert_eq!(native.thinking_budget, None);
    assert_eq!(native.thinking_keep, None);
    assert_ne!(native.reasoning_effort.as_deref(), Some("off"));
}
