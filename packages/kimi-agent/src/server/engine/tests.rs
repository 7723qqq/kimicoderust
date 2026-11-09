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

#[tokio::test]
async fn session_spec_publishes_the_session_roots_to_the_agent_manager() {
    let temp_dir = tempfile::tempdir().unwrap();
    let workspace = temp_dir.path().join("workspace");
    let extra = temp_dir.path().join("extra");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&extra).unwrap();

    let mut spec = spec();
    spec.workspace_root = Some(workspace.to_string_lossy().into_owned());
    spec.extra_roots = vec![
        extra.to_string_lossy().into_owned(),
        temp_dir
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned(),
    ];
    let engine = ServerEngine::new(
        spec,
        Arc::new(EventHub::new()),
        Arc::new(SqliteSessionStore::in_memory().unwrap()),
    );
    let workspace_row = engine
        .store()
        .create_workspace(workspace.to_str().unwrap(), None)
        .unwrap();
    engine
        .store()
        .create_session_with_workspace("sess-roots", None, Some(&workspace_row.id))
        .unwrap();

    // The turn's roots are the session's live workspace: workdir plus the
    // directories this session may touch. A missing extra root is dropped, the
    // same way the toolset drops it.
    engine.session_spec("sess-roots").await;
    let manager = engine.subagent_manager();
    let first = manager
        .session_workspace_context("sess-roots")
        .expect("the turn publishes the session roots")
        .snapshot();
    assert_eq!(first.revision, 1);
    assert_eq!(
        first.roots.work_dir(),
        std::fs::canonicalize(&workspace).unwrap()
    );
    assert!(
        first
            .roots
            .contains(&std::fs::canonicalize(&extra).unwrap())
    );

    // The next turn resolves the same set, so nothing is republished.
    engine.session_spec("sess-roots").await;
    assert_eq!(
        manager
            .session_workspace_context("sess-roots")
            .unwrap()
            .revision(),
        1
    );
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
        session_start: None,
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

#[tokio::test]
async fn the_session_provider_type_follows_the_resolved_model() {
    let engine = engine();
    let config: crate::config::KimiConfig = r#"
[providers.kimi]
type = "kimi"
api_key = "k"
base_url = "https://api.moonshot.cn/v1"

[models.test-model]
provider = "kimi"
model = "kimi-k2"
"#
    .parse()
    .unwrap();
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));
    assert_eq!(
        engine.session_provider_type("s1").await.as_deref(),
        Some("kimi"),
        "prompt intake must read the same provider the transport will use \
         (the Kimi image budget is wider than the baseline)"
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
    // A second service on the same directory stands in for another process.
    let foreign = crate::session::lock::SessionLockService::new(
        crate::session::lock::SESSION_LOCKS.lock_dir().to_path_buf(),
    );
    foreign.hold("sess-occupied-engine").unwrap();
    // A real foreign hold carries a foreign pid. A same-pid file is this
    // process's own leftover and is reclaimable by design, so rewrite the
    // payload to look like the other host it is standing in for.
    let lock_path = foreign.lock_dir().join("sess-occupied-engine.lock");
    let mut holder: crate::session::lock::SessionLockHolder =
        serde_json::from_str(&std::fs::read_to_string(&lock_path).unwrap()).unwrap();
    holder.pid = 999_999_999;
    holder.hostname = "other-host".into();
    std::fs::write(&lock_path, serde_json::to_string(&holder).unwrap()).unwrap();

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
        for _ in 0..10 {
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
            "session.meta.updated",        // first-prompt lastPrompt/title
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

    let crate::events::EngineEvent::Custom(start_phase) = &events[3].event else {
        panic!("expected the running phase, got {:?}", events[3].event);
    };
    assert_eq!(start_phase["phase"]["kind"], "running");

    let crate::events::EngineEvent::Custom(end_phase) = &events[9].event else {
        panic!("expected the ended phase, got {:?}", events[9].event);
    };
    assert_eq!(end_phase["phase"]["kind"], "ended");
    assert_eq!(end_phase["phase"]["reason"], "completed");

    // The turn closes on the typed event, scoped by its turn number and
    // carrying the loop's end reason — the v2 turn-lifecycle contract.
    let crate::events::EngineEvent::TurnEnded {
        turn_id, reason, ..
    } = &events[5].event
    else {
        panic!("expected turn.ended, got {:?}", events[5].event);
    };
    assert_eq!(turn_id, "turn-1");
    assert_eq!(reason, "completed");

    let crate::events::EngineEvent::SessionWorkChanged {
        busy,
        last_turn_reason,
        ..
    } = &events[6].event
    else {
        panic!("expected work_changed, got {:?}", events[6].event);
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
    // v2 `estimateTokensForMessage` counts the role plus the content: role
    // "user" (1) + four content bytes (1) = 2.
    assert_eq!(payload["contextTokens"], 2);

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
/// last request anchors the *input prefix* it served, so the reply appended
/// after it is estimated on top (v2 `measured` uses the request context
/// length, not the finished transcript's).
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
    let history = engine.store().load_session_history("sess-count").unwrap();
    // The loop appends its injected reminder as a user turn, so the stored
    // slice is [user, reminder, reply]; only the reply trails the request.
    assert_eq!(
        history.len(),
        3,
        "the user message, a reminder and the reply"
    );
    let reply = crate::session::token_counting::estimate_message(&history[2]);
    assert_eq!(
        engine.token_counting("sess-count").anchors[0].length,
        history.len() - 1,
        "the anchor covers the request input, not the reply"
    );
    assert_eq!(
        engine.session_context_tokens("sess-count"),
        measured + reply,
        "the measured prefix plus the estimated reply"
    );
    assert_eq!(
        engine.status_payload("sess-count")["contextTokens"],
        measured + reply,
        "the gauge carries the measured total and the estimated tail"
    );
}

/// v2 `matchesContext`: a request whose input prefix a compaction replaced no
/// longer anchors the conversation — the turn falls back to an estimate at
/// the final transcript length instead of pinning a stale measurement.
#[test]
fn a_spliced_request_measurement_falls_back_to_an_estimate() {
    let engine = engine();
    engine.store().create_session("sess-splice", None).unwrap();
    let messages = vec![
        crate::turn_loop::types::LLMMessage {
            role: "user".into(),
            content: "hello".into(),
            ..Default::default()
        },
        crate::turn_loop::types::LLMMessage {
            role: "assistant".into(),
            content: "done".into(),
            ..Default::default()
        },
    ];
    engine
        .store()
        .save_turn("sess-splice", "turn-1", 1, &messages, None, None)
        .unwrap();

    let usage = crate::rpc::types::TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        ..Default::default()
    };
    let stale = crate::turn_loop::types::RequestMeasurement {
        usage: usage.clone(),
        input_messages: 2,
        spliced_after: true,
    };
    engine.record_token_counting("sess-splice", Some(&stale), 0);
    let state = engine.token_counting("sess-splice");
    assert!(
        state.anchors.iter().all(|anchor| !anchor.measured),
        "a stale request must not install a measured anchor"
    );
    assert_eq!(
        engine.session_context_tokens("sess-splice"),
        crate::session::token_counting::estimate_messages(&messages),
        "the whole transcript is estimated"
    );

    // The same request, still current: two loop messages (system + user) map
    // to a one-message durable prefix, and the reply is estimated on top.
    let fresh = crate::turn_loop::types::RequestMeasurement {
        usage,
        input_messages: 2,
        spliced_after: false,
    };
    engine.record_token_counting("sess-splice", Some(&fresh), 0);
    assert_eq!(
        engine.session_context_tokens("sess-splice"),
        15 + crate::session::token_counting::estimate_message(&messages[1])
    );
}

/// The tool-schema table is part of every request, so the gauge counts it and
/// `should_compact` — the threshold the turn loop and the micro-compaction
/// path read — sizes the same figure. Enabling a large toolset must therefore
/// move the trigger, not leave it unaware of the schema.
#[test]
fn the_context_gauge_and_the_compaction_threshold_share_the_tool_schema_basis() {
    let engine = engine();
    engine.store().create_session("sess-schema", None).unwrap();

    engine.record_token_counting("sess-schema", None, 0);
    let without_schema = engine.session_context_tokens("sess-schema");
    engine.record_token_counting("sess-schema", None, 900);
    let with_schema = engine.session_context_tokens("sess-schema");
    assert_eq!(
        with_schema - without_schema,
        900,
        "the gauge adds the active tool schema"
    );

    let config = crate::compaction::CompactionConfig {
        max_context_tokens: 1_000,
        trigger_ratio: 0.5,
        reserved_context_size: 0,
        ..crate::compaction::config_for_window(Some(1_000))
    };
    assert!(
        !crate::compaction::should_compact(without_schema as u32, &config),
        "below the threshold without the schema"
    );
    assert!(
        crate::compaction::should_compact(with_schema as u32, &config),
        "the threshold reads the same gauge figure, so the schema moves it"
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
    assert_eq!(engine.session_context_tokens("sess-cut"), 24);

    let undone = engine.store().undo_turns("sess-cut", 1).expect("undo");
    assert_eq!(undone, 1);
    engine.record_context_truncation("sess-cut");
    // The messages are gone, but the request still carries the turn's tool
    // schema — the gauge counts it on the same basis as the compaction
    // trigger.
    let schema_tokens = engine.token_counting("sess-cut").tool_schema_tokens;
    assert!(schema_tokens > 0, "the scripted turn enables a toolset");
    assert_eq!(
        engine.session_context_tokens("sess-cut"),
        schema_tokens,
        "only the tool-schema overhead remains"
    );
    assert_eq!(
        engine.status_payload("sess-cut")["contextTokens"],
        schema_tokens
    );
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

/// v2 `WireService.externallyModified` / `SessionRunGateService.begin()`: a
/// journal that moved since this engine last saw it means another client wrote
/// the session while it was idle. A session never observed is a fresh scope
/// (nothing is stale), and adopting the head is the reload that clears it.
#[test]
fn a_journal_that_moved_since_the_observed_head_reads_as_externally_modified() {
    let engine = engine();
    engine.store().create_session("sess-stale", None).unwrap();

    assert!(
        !engine.journal_moved("sess-stale"),
        "a session this engine has never observed has no watermark to differ from"
    );
    let head = engine.observe_journal_head("sess-stale");
    assert!(!engine.journal_moved("sess-stale"));

    engine
        .store()
        .append_wire_event(&crate::native::event_store::RawWireEvent {
            id: "evt-foreign".into(),
            session_id: "sess-stale".into(),
            event_type: "message.user".into(),
            payload: serde_json::json!({ "content": "written by another client" }),
            is_checkpoint: false,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();
    assert!(
        engine.journal_moved("sess-stale"),
        "a foreign append after the watermark must read as an external modification"
    );

    assert!(engine.observe_journal_head("sess-stale") > head);
    assert!(
        !engine.journal_moved("sess-stale"),
        "adopting the head (the reload the stale error asks for) clears the doubt"
    );
}

/// Rows this process's own persister wrote are the engine's in-flight tail, not
/// an external modification. The turn watermark is taken before its post-turn
/// events and the next prompt's `prompt.submitted` are persisted, so without
/// the persisted head every second turn in the same process would be refused as
/// `session.stale` — before its step head could drain a surviving steer.
#[test]
fn a_row_this_process_persisted_is_not_an_external_modification() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-own-tail", None)
        .unwrap();
    engine.observe_journal_head("sess-own-tail");

    let seq = engine
        .store()
        .append_wire_event(&crate::native::event_store::RawWireEvent {
            id: "evt-own-tail".into(),
            session_id: "sess-own-tail".into(),
            event_type: "turn.ended".into(),
            payload: serde_json::json!({ "reason": "cancelled" }),
            is_checkpoint: true,
            is_compaction: false,
            created_at: 1,
        })
        .unwrap();
    assert!(
        engine.journal_moved("sess-own-tail"),
        "a row written behind the engine's back is foreign until the persister claims it"
    );
    engine.hub.persisted_heads().note("sess-own-tail", seq);
    assert!(
        !engine.journal_moved("sess-own-tail"),
        "the persister's own row is this process's in-flight tail"
    );
}

#[test]
fn agents_md_reminder_lists_stale_and_new_files() {
    use crate::environment::AgentsMdChange;

    let changes = vec![
        AgentsMdChange::Modified {
            path: std::path::PathBuf::from("/repo/AGENTS.md"),
        },
        AgentsMdChange::Removed {
            path: std::path::PathBuf::from("/repo/old/AGENTS.md"),
        },
        AgentsMdChange::Added {
            path: std::path::PathBuf::from("/repo/pkg/AGENTS.md"),
        },
    ];
    let text = agents_md_change_reminder(&changes).expect("reminder");
    assert!(text.contains("/repo/AGENTS.md"));
    assert!(text.contains("/repo/old/AGENTS.md (deleted)"));
    assert!(text.contains("/repo/pkg/AGENTS.md"));
    assert!(text.contains("the copies injected in the system prompt are stale"));
}

#[test]
fn agents_md_reminder_is_silent_without_changes() {
    assert!(agents_md_change_reminder(&[]).is_none());
}

// ── `[environments]` at session creation ─────────────────────────────

fn environment_entry(kind: &str) -> crate::environment::EnvironmentEntry {
    crate::environment::EnvironmentEntry {
        kind: Some(kind.to_string()),
        ..Default::default()
    }
}

/// An engine whose config carries `environments`, plus a workspace-backed
/// session created before the environment is frozen.
async fn engine_with_environments(
    environments: crate::environment::EnvironmentsConfig,
) -> (tempfile::TempDir, ServerEngine) {
    let temp_dir = tempfile::tempdir().unwrap();
    let workspace = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let mut spec = spec();
    spec.workspace_root = Some(workspace.to_string_lossy().into_owned());
    let engine = ServerEngine::new(
        spec,
        Arc::new(EventHub::new()),
        Arc::new(SqliteSessionStore::in_memory().unwrap()),
    );
    let mut config = crate::config::KimiConfig::default();
    config.environments = environments;
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(config))));

    let workspace_row = engine
        .store()
        .create_workspace(workspace.to_str().unwrap(), None)
        .unwrap();
    engine
        .store()
        .create_session_with_workspace("sess-env", None, Some(&workspace_row.id))
        .unwrap();
    (temp_dir, engine)
}

/// Declaring a remote entry does not poison creation: with no `default`, the
/// session binds to the built-in local environment, exactly like a config with
/// no `[environments]` section.
#[tokio::test]
async fn a_declared_environment_without_a_default_still_binds_local() {
    let (temp_dir, engine) = engine_with_environments(crate::environment::EnvironmentsConfig {
        default: None,
        entries: std::collections::BTreeMap::from([("dev".to_string(), environment_entry("ssh"))]),
    })
    .await;

    let snapshot = engine
        .freeze_session_environment("sess-env")
        .await
        .unwrap()
        .expect("a workspace-backed session freezes a snapshot");
    assert_eq!(snapshot.environment_id(), "local");
    assert_eq!(
        snapshot.cwd(),
        std::fs::canonicalize(temp_dir.path().join("workspace")).unwrap()
    );
}

/// A declared default is what upstream uses when a session names no
/// environment; this engine cannot reach the transport, so creation fails with
/// the transport named rather than silently running locally.
#[tokio::test]
async fn a_declared_default_is_refused_as_unavailable_on_creation() {
    let (_temp_dir, engine) = engine_with_environments(crate::environment::EnvironmentsConfig {
        default: Some("dev".to_string()),
        entries: std::collections::BTreeMap::from([("dev".to_string(), environment_entry("ssh"))]),
    })
    .await;

    let error = engine
        .freeze_session_environment("sess-env")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        crate::environment::codes::ENVIRONMENT_UNAVAILABLE
    );
    assert!(error.detail().contains("'dev'"), "{}", error.detail());
    assert!(error.detail().contains("ssh"), "{}", error.detail());
}

/// `local` is built in, not a declarable transport (upstream's entry union is
/// ssh/docker/command): a config that declares it is malformed and creation
/// reports that, instead of pretending the transport is merely unavailable.
///
/// This is the create/restore backstop: the config loader already staged the
/// same finding as an `event.config.warning`, but the file can be edited while
/// the server runs, so the typed error must still be reachable here.
#[tokio::test]
async fn a_local_declaration_fails_creation_with_a_declaration_error() {
    let (_temp_dir, engine) = engine_with_environments(crate::environment::EnvironmentsConfig {
        default: None,
        entries: std::collections::BTreeMap::from([(
            "dev".to_string(),
            environment_entry("local"),
        )]),
    })
    .await;

    let error = engine
        .freeze_session_environment("sess-env")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        crate::environment::codes::ENVIRONMENT_DECLARATION_INVALID
    );
    assert!(error.detail().contains("'dev'"), "{}", error.detail());
    assert!(error.detail().contains("built in"), "{}", error.detail());
}

/// A dangling `default` stays a config error at the same moment upstream
/// reports it: while the declaration is validated, before any snapshot.
#[tokio::test]
async fn a_dangling_default_fails_creation_with_a_declaration_error() {
    let (_temp_dir, engine) = engine_with_environments(crate::environment::EnvironmentsConfig {
        default: Some("missing".to_string()),
        entries: std::collections::BTreeMap::new(),
    })
    .await;

    let error = engine
        .freeze_session_environment("sess-env")
        .await
        .unwrap_err();
    assert_eq!(
        error.code(),
        crate::environment::codes::ENVIRONMENT_DECLARATION_INVALID
    );
    assert!(error.detail().contains("missing"), "{}", error.detail());
}

/// A scripted LLM that reports one usage per call, so a multi-turn test can
/// drive the cache-read series the tracker observes.
struct ScriptedUsageLlm {
    calls: std::sync::atomic::AtomicUsize,
    usages: Vec<TokenUsage>,
}

impl LLM for ScriptedUsageLlm {
    fn system_prompt(&self) -> &str {
        "sys"
    }
    fn model_name(&self) -> &str {
        "cache-model"
    }
    fn is_retryable_error(&self, _error: &str) -> bool {
        false
    }
    fn chat(
        &self,
        _params: LLMChatParams,
    ) -> BoxFuture<'_, Result<LLMChatResponse, Box<dyn std::error::Error + Send + Sync>>> {
        let index = self
            .calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .min(self.usages.len().saturating_sub(1));
        let usage = self.usages.get(index).cloned().unwrap_or_default();
        Box::pin(async move {
            Ok(LLMChatResponse {
                content: "done".into(),
                thinking: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage,
                timing: None,
            })
        })
    }
}

fn cache_usage(input_other: u32, cache_read: u32, cache_creation: u32, output: u32) -> TokenUsage {
    TokenUsage {
        input_tokens: input_other,
        output_tokens: output,
        total_tokens: input_other + cache_read + cache_creation + output,
        input_cache_read: cache_read,
        input_cache_creation: cache_creation,
    }
}

/// D14: a collapsing cache read is recorded once with its buckets; a
/// sub-threshold dip afterwards adds no noise. The read entry exposes what was
/// collected without any wire field.
#[tokio::test]
async fn a_cache_read_collapse_is_recorded_and_a_small_dip_is_not() {
    let engine = engine();
    engine.store().create_session("sess-cache", None).unwrap();
    let llm = ScriptedUsageLlm {
        calls: std::sync::atomic::AtomicUsize::new(0),
        usages: vec![
            cache_usage(10, 10_000, 0, 10),
            cache_usage(5_000, 2_000, 0, 10),
            cache_usage(5_000, 1_900, 0, 10),
        ],
    };

    engine
        .run_turn_on(&llm, "sess-cache", 1, Vec::new(), "first")
        .await
        .expect("first turn completes");
    let snapshot = engine.cache_observation("sess-cache");
    assert_eq!(
        snapshot["breaks"].as_array().map(Vec::len),
        Some(0),
        "the first observation only seeds the baseline"
    );
    assert_eq!(snapshot["last"]["cache_read"], 10_000);
    assert_eq!(snapshot["last"]["input_tokens"], 10_010);
    assert_eq!(
        snapshot["last"]["message_count"], 3,
        "system + user + the loop's reminder"
    );
    assert_eq!(snapshot["baseline"]["input_cache_read"], 10_000);

    let history = engine.store().load_session_history("sess-cache").unwrap();
    engine
        .run_turn_on(&llm, "sess-cache", 2, history, "second")
        .await
        .expect("second turn completes");
    let snapshot = engine.cache_observation("sess-cache");
    let breaks = snapshot["breaks"].as_array().expect("breaks is an array");
    assert_eq!(breaks.len(), 1, "one collapse, one record");
    assert_eq!(breaks[0]["cause"], "unexplained");
    assert_eq!(breaks[0]["previous"]["input_cache_read"], 10_000);
    assert_eq!(breaks[0]["current"]["input_cache_read"], 2_000);

    let history = engine.store().load_session_history("sess-cache").unwrap();
    engine
        .run_turn_on(&llm, "sess-cache", 3, history, "third")
        .await
        .expect("third turn completes");
    assert_eq!(
        engine.cache_observation("sess-cache")["breaks"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "2000 -> 1900 is exactly the 95% boundary and must stay silent"
    );
}

/// D14: an unmeasured step is not a break and does not move the baseline, so
/// providers that report nothing cannot fabricate a collapse.
#[test]
fn an_unmeasured_cache_observation_does_not_move_the_baseline() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-cache-unmeasured", None)
        .unwrap();
    let observation = crate::session::cache_observation::build_cache_observation(
        crate::session::cache_observation::CacheObservationInput {
            model_alias: "cache-model",
            protocol: None,
            provider_type: None,
            message_count: 2,
            history_hash: "h",
            projection: None,
            thinking_effort: None,
            usage: Some(&cache_usage(10, 10_000, 0, 10)),
        },
    );
    engine.observe_cache_observations(
        "sess-cache-unmeasured",
        "turn-1",
        crate::callbacks::MAIN_AGENT_ID,
        std::slice::from_ref(&observation),
        &[],
        None,
    );
    let empty = crate::rpc::types::TokenUsage::default();
    let unmeasured = crate::session::cache_observation::build_cache_observation(
        crate::session::cache_observation::CacheObservationInput {
            model_alias: "cache-model",
            protocol: None,
            provider_type: None,
            message_count: 3,
            history_hash: "h2",
            projection: None,
            thinking_effort: None,
            usage: Some(&empty),
        },
    );
    engine.observe_cache_observations(
        "sess-cache-unmeasured",
        "turn-2",
        crate::callbacks::MAIN_AGENT_ID,
        std::slice::from_ref(&unmeasured),
        &[],
        None,
    );
    let snapshot = engine.cache_observation("sess-cache-unmeasured");
    assert_eq!(snapshot["breaks"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        snapshot["baseline"]["input_cache_read"], 10_000,
        "the unmeasured step must not replace the reference"
    );
}

/// D14: a compaction rebase drops every pre-compaction comparison datum (the
/// break baseline, the last observation, the request snapshots) so the summary
/// prefix is never measured against the old transcript; the recorded breaks
/// are history and stay.
#[test]
fn a_compaction_resets_the_cache_break_baseline() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-cache-reset", None)
        .unwrap();
    let observation = crate::session::cache_observation::build_cache_observation(
        crate::session::cache_observation::CacheObservationInput {
            model_alias: "cache-model",
            protocol: None,
            provider_type: None,
            message_count: 2,
            history_hash: "h",
            projection: None,
            thinking_effort: None,
            usage: Some(&cache_usage(10, 10_000, 0, 10)),
        },
    );
    engine.observe_cache_observations(
        "sess-cache-reset",
        "turn-1",
        crate::callbacks::MAIN_AGENT_ID,
        std::slice::from_ref(&observation),
        &[],
        Some(&crate::session::cache_observation::CacheProbeRequest {
            model_alias: "cache-model".into(),
            tools_hash: "tools".into(),
            system_prompt_hash: "prompt".into(),
            message_count: 2,
            history_hash: "h".into(),
            input_tokens: Some(10_020),
            started_at: 1,
            finished_at: 2,
        }),
    );
    assert_eq!(
        engine.cache_observation("sess-cache-reset")["baseline"]["input_cache_read"],
        10_000
    );
    assert!(
        engine.cache_observation("sess-cache-reset")["requests"]
            .get(crate::callbacks::MAIN_AGENT_ID)
            .is_some()
    );

    engine.record_context_rebase("sess-cache-reset");
    let snapshot = engine.cache_observation("sess-cache-reset");
    assert!(snapshot["baseline"].is_null(), "the baseline was dropped");
    assert!(
        snapshot["last"].is_null(),
        "the pre-compaction observation must not label the summary prefix"
    );
    assert_eq!(
        snapshot["requests"]
            .as_object()
            .map(|requests| requests.len()),
        Some(0),
        "the pre-compaction request snapshots must not be compared with the summary prefix"
    );
}

/// D14: a turn that compacted mid-loop hands the engine the observation index
/// where the history was replaced. The fold resets the break baseline there, so
/// the post-compaction cache-read drop is not reported as a break (v2 resets on
/// `context.compactionCompleted`).
#[test]
fn a_compaction_boundary_in_the_fold_resets_the_baseline() {
    let engine = engine();
    engine
        .store()
        .create_session("sess-fold-reset", None)
        .unwrap();
    let before_usage = cache_usage(10, 10_000, 0, 10);
    let before = crate::session::cache_observation::build_cache_observation(
        crate::session::cache_observation::CacheObservationInput {
            model_alias: "cache-model",
            protocol: None,
            provider_type: None,
            message_count: 6,
            history_hash: "before",
            projection: None,
            thinking_effort: None,
            usage: Some(&before_usage),
        },
    );
    let after_usage = cache_usage(10, 1_000, 0, 10);
    let after = crate::session::cache_observation::build_cache_observation(
        crate::session::cache_observation::CacheObservationInput {
            model_alias: "cache-model",
            protocol: None,
            provider_type: None,
            message_count: 2,
            history_hash: "after",
            projection: None,
            thinking_effort: None,
            usage: Some(&after_usage),
        },
    );
    engine.observe_cache_observations(
        "sess-fold-reset",
        "turn-1",
        crate::callbacks::MAIN_AGENT_ID,
        &[before, after],
        &[1],
        None,
    );
    let snapshot = engine.cache_observation("sess-fold-reset");
    assert_eq!(
        snapshot["breaks"].as_array().map(Vec::len),
        Some(0),
        "the read drop across the compaction boundary is not a break"
    );
    assert_eq!(
        snapshot["baseline"]["input_cache_read"], 1_000,
        "the post-compaction observation becomes the new baseline"
    );
}

/// D10: the engine reads the configured strategy through the config source, so
/// an explicit `measured`/`estimated` changes the gauge and the default is
/// byte-for-byte the historical figure.
#[test]
fn the_context_gauge_honours_the_configured_token_counting_strategy() {
    use std::str::FromStr;
    // The gauge reads `KIMI_TOKEN_COUNTING_STRATEGY` on every call, so the
    // whole test must own the variable: taking the lock only for the last
    // assertion let a parallel config test flip the value under the earlier
    // ones.
    let _env = crate::session::token_counting::tests::env_lock();
    unsafe { std::env::remove_var("KIMI_TOKEN_COUNTING_STRATEGY") };
    let engine = engine();
    engine
        .store()
        .create_session("sess-strategy", None)
        .unwrap();
    let messages = vec![
        crate::turn_loop::types::LLMMessage {
            role: "user".into(),
            content: "hello".into(),
            ..Default::default()
        },
        crate::turn_loop::types::LLMMessage {
            role: "assistant".into(),
            content: "hi".into(),
            ..Default::default()
        },
    ];
    engine
        .store()
        .save_turn("sess-strategy", "turn-1", 1, &messages, None, None)
        .unwrap();
    let measurement = crate::turn_loop::types::RequestMeasurement {
        usage: cache_usage(100, 0, 0, 5),
        input_messages: messages.len(),
        spliced_after: false,
    };
    engine.record_token_counting("sess-strategy", Some(&measurement), 0);

    let tail = crate::session::token_counting::estimate_message(&messages[1]);
    let composed = 105 + tail;
    let estimated = crate::session::token_counting::estimate_messages(&messages);
    assert_eq!(engine.session_context_tokens("sess-strategy"), composed);
    assert_eq!(
        engine.status_payload("sess-strategy")["contextTokens"],
        composed
    );

    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(
        crate::config::KimiConfig::from_str("[tokenCounting]\nstrategy = \"measured\"\n").unwrap(),
    ))));
    assert_eq!(
        engine.session_context_tokens("sess-strategy"),
        105,
        "measured ignores the estimated tail"
    );

    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(Some(
        crate::config::KimiConfig::from_str("[tokenCounting]\nstrategy = \"estimated\"\n").unwrap(),
    ))));
    assert_eq!(
        engine.session_context_tokens("sess-strategy"),
        estimated,
        "estimated is the pure estimator over the stored history"
    );

    // Back to the default: no config source at all.
    engine.set_config_source(Arc::new(tokio::sync::Mutex::new(None)));
    assert_eq!(engine.session_context_tokens("sess-strategy"), composed);
}

/// D10: a read that races a config reload must not report the default. The
/// async config source is briefly held while a reload swaps it, and the
/// synchronous gauge used to treat `try_lock` contention as "unconfigured".
#[tokio::test]
async fn a_contended_config_read_does_not_downgrade_the_token_counting_strategy() {
    use std::str::FromStr;
    let _env = crate::session::token_counting::tests::env_lock();
    unsafe { std::env::remove_var("KIMI_TOKEN_COUNTING_STRATEGY") };
    let engine = engine();
    let source = Arc::new(tokio::sync::Mutex::new(Some(
        crate::config::KimiConfig::from_str("[tokenCounting]\nstrategy = \"measured\"\n").unwrap(),
    )));
    engine.set_config_source(Arc::clone(&source));
    assert_eq!(
        engine.token_counting_strategy(),
        crate::session::token_counting::TokenCountingStrategy::Measured
    );

    // Stand in for a reload: hold the source while the gauge reads it.
    let guard = source.lock().await;
    assert_eq!(
        engine.token_counting_strategy(),
        crate::session::token_counting::TokenCountingStrategy::Measured,
        "a contended read must reuse the last resolved strategy, not the default"
    );
    drop(guard);
}
