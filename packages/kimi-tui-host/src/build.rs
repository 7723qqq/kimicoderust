//! Assembling a session from the user's configuration.
//!
//! This is the composition root for the terminal host. It deliberately does
//! less than the REPL's: no cron scheduler, no tower wake driver, no image
//! preview wrapper. Every one of those is a capability the REPL shows in
//! scrollback and the widget stack has no surface for yet, and wiring them
//! twice would mean keeping two compositions in step.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use anyhow::{Result, anyhow};
use kimi_agent::callbacks::{CountingCallbacks, HostCallbacks, NativeToolCallbacks};
use kimi_agent::config::{KimiConfig, ResolvedNativeLlm};
use kimi_agent::events::{EventBus, Subscription};
use kimi_agent::llm::http::NativeHttpLlm;
use kimi_agent::mcp::McpManager;
use kimi_agent::permission::PermissionEngine;
use kimi_agent::session::{EngineSession, SessionConfig};
use kimi_agent::storage::StateStore;
use kimi_agent::tool_result_truncation::ToolResultTruncator;
use kimi_agent::tools::{self, NativeToolset};
use kimi_agent::turn_loop::types::LLM;
use kimi_tui_app::{UiCommand, UiState};

use crate::host::TuiHostCallbacks;
use crate::transport::{EngineTransport, project_events};

pub struct BuiltEngine {
    pub session: EngineSession,
    pub state: Arc<UiState>,
    /// The mode the session's policy snapshot resolved to. The terminal has no
    /// approval dialog yet, so the banner says so out loud instead of letting a
    /// `manual` session silently approve everything.
    pub permission_mode: kimi_agent::permission::PermissionMode,
    /// The UI → engine channel, drained by [`crate::drive_commands`].
    pub commands: tokio::sync::mpsc::UnboundedReceiver<UiCommand>,
    /// The event fold, shared with [`crate::drive_commands`]: a steer allocates
    /// its row through the same projector that renders the turn, so the row and
    /// the streamed text keep one id order.
    pub bridge: Arc<std::sync::Mutex<crate::transport::Bridge>>,
    /// Every steer hop, shared by the engine and the host. A reader can ask
    /// which stage a prompt is at instead of inferring it from the transcript.
    pub steer_trace: kimi_ui_core::SteerJournal,
    /// Kept alive so the event feed keeps its subscription.
    _events: Subscription,
}

/// Build the session the terminal drives, and subscribe the UI state to its
/// events.
pub async fn build_engine(
    config: KimiConfig,
    workspace: PathBuf,
    model: Option<String>,
    profile_name: Option<String>,
) -> Result<BuiltEngine> {
    let model = model
        .or_else(|| config.default_model.clone())
        .unwrap_or_else(|| "default".to_string());
    let resolved = config
        .extract_native_llm(Some(&model))
        .ok_or_else(|| anyhow!("no valid provider/key for model '{model}'"))?;

    // The constant below is the fallback for installs that resolve a model without
    // a declared system prompt. Where possible, build the real prompt from the
    // workspace so skills, AGENTS.md and memory reach the model the same way they
    // do in the CLI.
    let default_system_prompt = match profile_name {
        Some(profile) if !profile.is_empty() => {
            kimi_agent::prompt::SystemPromptBuilder::build_for_profile(
                &workspace,
                config.extra_skill_dirs_paths(),
                config.resolve_merge_all_available_skills(),
                profile,
            )
        }
        _ => kimi_agent::prompt::SystemPromptBuilder::build_default_with_skill_config(
            &workspace,
            config.extra_skill_dirs_paths(),
            config.resolve_merge_all_available_skills(),
        ),
    };
    let system_prompt = resolved
        .system_prompt
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(default_system_prompt);
    let bus = Arc::new(EventBus::new());
    // The model's streamed text has no other way into the transcript: the
    // transport reports `llm.delta` to the sink it was built with, and the
    // projector folds those into the assistant entry as they arrive.
    let sink_bus = Arc::clone(&bus);
    let llm: Arc<dyn LLM> = Arc::new(
        NativeHttpLlm::new(config.native_llm_config(&resolved), system_prompt)
            .with_sink(Arc::new(move |event| sink_bus.publish_json(event))),
    );
    build_session(config, workspace, resolved, llm, bus).await
}

/// The same composition with the model transport and the event bus supplied by
/// the caller.
///
/// [`build_engine`] is this function plus the provider resolution and the
/// transport's delta sink, so an entry point that already owns an `LLM` — the
/// integration test that drives the slice against a stub — gets the identical
/// session wiring instead of a parallel copy of it.
pub async fn build_session(
    config: KimiConfig,
    workspace: PathBuf,
    resolved: ResolvedNativeLlm,
    llm: Arc<dyn LLM>,
    bus: Arc<EventBus>,
) -> Result<BuiltEngine> {
    let state_store = Arc::new(StateStore::for_workspace(&workspace)?);
    let policy = config.build_policy_snapshot(Some(workspace.clone()));
    let permission_mode = policy.mode;
    let permission_engine = Arc::new(PermissionEngine::new(policy));
    let truncator = Arc::new(ToolResultTruncator::for_workspace(&workspace));

    let mcp = Arc::new(McpManager::new());
    mcp.spawn_from_config(&config).await;
    let github = kimi_agent::tools::github::GitHubCredentials {
        token: config.github.token.clone(),
        base_url: config.github.base_url.clone(),
    };

    let steer_trace = kimi_ui_core::SteerJournal::new();
    let (transport, commands) = EngineTransport::channel();
    let state = UiState::new(transport);
    let base: Arc<dyn HostCallbacks> = Arc::new(TuiHostCallbacks::new(
        Arc::clone(&state_store),
        Arc::clone(&state),
    ));
    let event_count = Arc::new(AtomicU32::new(0));
    let counting: Arc<dyn HostCallbacks> =
        Arc::new(CountingCallbacks::new(base, event_count).with_bus(Arc::clone(&bus)));

    let toolset = Arc::new(
        NativeToolset::new(&workspace.to_string_lossy(), None)
            .ok_or_else(|| anyhow!("invalid workspace root: {}", workspace.display()))?
            .with_callbacks(Arc::clone(&counting))
            .with_mcp(Arc::clone(&mcp))
            .with_permission_mode(permission_mode)
            .with_github_credentials(github.clone())
            .with_model_capabilities(resolved.capabilities.clone()),
    );
    let native_count = Arc::new(AtomicU32::new(0));
    let callbacks: Arc<dyn HostCallbacks> = Arc::new(NativeToolCallbacks::for_standalone(
        Arc::clone(&counting),
        Arc::clone(&toolset),
        native_count,
        Some(permission_engine),
        Some(truncator),
    ));

    let tool_defs_callbacks = Arc::clone(&callbacks);
    let session = EngineSession::new(SessionConfig {
        llm,
        callbacks,
        max_steps: config.resolve_max_steps_per_turn().unwrap_or(25),
        max_attempts: config.resolve_max_attempts_per_step(),
        max_context_tokens: None,
        steer_trace: Some(steer_trace.clone()),
        compaction_max_attempts: config.resolve_compaction_max_attempts(),
        permission_mode: Some(permission_mode),
        tool_defs: Arc::new(move || {
            let callbacks = Arc::clone(&tool_defs_callbacks);
            Box::pin(async move {
                // The callbacks layer's aggregate: the engine's own table with
                // the `[tools]` switch applied, the host's tools and MCP's,
                // deduplicated. Asking it is what keeps this host's table the
                // same one the REPL and the stdio server advertise, instead of
                // a third hand-built list that drifts.
                match callbacks.list_tools().await {
                    Ok(response) if !response.tools.is_empty() => response.tools,
                    _ => tools::core_tool_defs::core_tool_defs(),
                }
            })
        }),
        // The terminal host has no goal provider and no checkpoint hook yet;
        // the engine's defaults cover both. The steer slot is live so a
        // blocking `WaitFor` ends early when the user steers mid-turn.
        goal: None,
        on_before_turn: None,
        agent_cancel_slot: None,
        steer_slot: Some(Arc::new(std::sync::Mutex::new(None))),
        hook_guard: None,
        print_background: None,
        session_id: None,
        task_runner: None,
        toolset: Some(Arc::clone(&toolset)),
        telemetry: None,
    })
    .await;

    let events = project_events(&bus, Arc::clone(&state), steer_trace.clone());

    Ok(BuiltEngine {
        session,
        state,
        bridge: events.1,
        steer_trace,
        permission_mode,
        commands,
        _events: events.0,
    })
}
