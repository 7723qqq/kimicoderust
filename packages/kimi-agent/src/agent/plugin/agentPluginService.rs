use std::collections::HashSet;
use std::fmt::{Display, Formatter};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::json;

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::log::log::ILogService;
use crate::_base::utils::xml_escape::escape_xml_attr;
use crate::agent::context_memory::context_memory::IAgentContextMemoryService;
use crate::agent::context_memory::types::{ContextMessage, PromptOrigin};
use crate::agent::plugin::agent_plugin::IAgentPluginService;
use crate::agent::plugin::agent_plugin_ops::{
    PLUGIN_SESSION_START_SNAPSHOT_KEY, PluginSessionStartEvent, PluginSessionStartEventPayload,
    PluginSessionStartSnapshotState,
};
use crate::app::plugin::plugin::IPluginService;
use crate::app::plugin::types::{EnabledPluginSessionStart, PluginMutation, PluginMutationKind};
use crate::features::reminder::reminderService::ReminderService;
use crate::features::reminder::systemReminder::system_reminder_content;
use crate::features::reminder::types::{
    ContextInjectionContent, ContextInjectionContext, ContextInjectionResult, ReminderNotification,
};
use crate::features::skill::catalog::skillSource::PLUGIN_SKILL_SOURCE_ID;
use crate::features::skill::catalog::types::{SkillCatalog, SkillDefinition};
use crate::features::skill::session::skillCatalog::ISessionSkillCatalog;
use crate::human::llm::message::ContentPart;
use crate::state::eventDispatcher::IEventDispatcher;
use crate::wire::record::WireAsyncError;

const SESSION_START_INJECTION_VARIANT: &str = "plugin_session_start";
const PLUGIN_CHANGE_INJECTION_VARIANT: &str = "plugin_change";
const MAIN_AGENT_ID: &str = "main";
const SUPERSEDES_SUFFIX: &str =
    "This supersedes any earlier plugin_session_start reminder in this session.";
const NO_ACTIVE_SESSION_STARTS: &str = "There are currently no active plugin session starts. This supersedes any earlier plugin_session_start reminder in this session.";
pub const PLUGIN_SESSION_START_REFRESH_PENDING_KEY: &str =
    "agentPlugin.sessionStartRefreshPending";

#[derive(Clone)]
pub struct AgentPluginService {
    agent_id: String,
    reminder: Arc<dyn ReminderService>,
    context: Arc<dyn IAgentContextMemoryService>,
    plugins: Arc<dyn IPluginService>,
    skill_catalog: Arc<dyn ISessionSkillCatalog>,
    session_id: String,
    log: Arc<dyn ILogService>,
    dispatcher: Arc<dyn IEventDispatcher>,
    snapshot: Arc<Mutex<PluginSessionStartSnapshotState>>,
    refresh_pending: Arc<AtomicBool>,
    warned_missing_session_start_skills: Arc<Mutex<HashSet<String>>>,
    pending_mutation_catalog_changes: Arc<AtomicUsize>,
    subscriptions: Arc<Mutex<Vec<DisposableRef>>>,
    unsubscribers: Arc<Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>>,
}

impl AgentPluginService {
    pub fn new(
        agent_id: String,
        reminder: Arc<dyn ReminderService>,
        context: Arc<dyn IAgentContextMemoryService>,
        plugins: Arc<dyn IPluginService>,
        skill_catalog: Arc<dyn ISessionSkillCatalog>,
        session_id: String,
        log: Arc<dyn ILogService>,
        dispatcher: Arc<dyn IEventDispatcher>,
    ) -> Self {
        std::sync::LazyLock::force(&PLUGIN_SESSION_START_SNAPSHOT_KEY);
        let service = Self {
            agent_id,
            reminder,
            context,
            plugins,
            skill_catalog,
            session_id,
            log,
            dispatcher,
            snapshot: Arc::new(Mutex::new(PluginSessionStartSnapshotState::default())),
            refresh_pending: Arc::new(AtomicBool::new(false)),
            warned_missing_session_start_skills: Arc::new(Mutex::new(HashSet::new())),
            pending_mutation_catalog_changes: Arc::new(AtomicUsize::new(0)),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
            unsubscribers: Arc::new(Mutex::new(Vec::new())),
        };
        if service.agent_id == MAIN_AGENT_ID {
            service.attach();
        }
        service
    }

    fn attach(&self) {
        let service = self.clone();
        let unregister = self.reminder.register(
            SESSION_START_INJECTION_VARIANT,
            Arc::new(move |injection| {
                let service = service.clone();
                Box::pin(async move {
                    let desired = service.reconcile_session_start_reminder(injection).await?;
                    Ok(desired.map(|content| ContextInjectionResult {
                        content: ContextInjectionContent::Text(content),
                        disclosure: None,
                    }))
                })
            }),
        );
        self.unsubscribers.lock().push(unregister);
        let service = self.clone();
        let subscription = self.skill_catalog.on_did_change().subscribe(move |source_id| {
            if source_id != PLUGIN_SKILL_SOURCE_ID {
                return;
            }
            if service
                .pending_mutation_catalog_changes
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                    Some(value.saturating_sub(1))
                })
                .unwrap_or(0)
                > 0
            {
                return;
            }
            service.refresh_pending.store(true, Ordering::Relaxed);
        });
        self.subscriptions.lock().push(Arc::new(subscription));
        let service = self.clone();
        let subscription = self.plugins.on_did_mutate(Arc::new(move |summary| {
            service
                .pending_mutation_catalog_changes
                .fetch_add(1, Ordering::Relaxed);
            service.reminder.notify(
                &render_plugin_change_reminder(&summary.mutation),
                &ReminderNotification {
                    variant: PLUGIN_CHANGE_INJECTION_VARIANT.to_string(),
                    owner_prompt_id: None,
                },
            );
        }));
        self.subscriptions.lock().push(subscription);
    }

    fn refresh_pending(&self) -> bool {
        self.refresh_pending.load(Ordering::Relaxed)
    }

    async fn render_session_start_reminder(&self) -> Result<Option<String>, WireAsyncError> {
        let session_starts = self.plugins.enabled_session_starts().await?;
        if session_starts.is_empty() {
            return Ok(None);
        }
        self.skill_catalog.ready().await.map_err(PluginError)?;
        Ok(render_plugin_session_start_reminder(
            &session_starts,
            self.skill_catalog.catalog().as_ref(),
            Some(self.log.clone()),
            Some(&self.session_id),
            &self.warned_missing_session_start_skills,
        ))
    }

    async fn reconcile_session_start_reminder(
        &self,
        injection: ContextInjectionContext,
    ) -> Result<Option<String>, WireAsyncError> {
        let force_refresh = self.refresh_pending();
        let desired = self.resolve_desired_session_start(&injection, force_refresh).await?;
        self.refresh_pending.store(false, Ordering::Relaxed);
        let latest = injection.last_injection.as_ref();
        let Some(desired) = desired else {
            if latest.is_none()
                && (!force_refresh || !should_neutralize_plugin_session_start(&self.context.get()))
            {
                return Ok(None);
            }
            if latest
                .and_then(latest_system_reminder_content)
                .is_some_and(|content| content == NO_ACTIVE_SESSION_STARTS)
            {
                return Ok(None);
            }
            return Ok(Some(NO_ACTIVE_SESSION_STARTS.to_string()));
        };
        let Some(latest) = latest else {
            return Ok(Some(desired));
        };
        let rendered = latest_system_reminder_content(latest).unwrap_or("");
        if !force_refresh
            && (rendered == desired.trim()
                || rendered == format!("{desired}\n\n{SUPERSEDES_SUFFIX}").trim())
        {
            return Ok(None);
        }
        Ok(Some(format!("{desired}\n\n{SUPERSEDES_SUFFIX}")))
    }

    async fn resolve_desired_session_start(
        &self,
        injection: &ContextInjectionContext,
        force_refresh: bool,
    ) -> Result<Option<String>, WireAsyncError> {
        if !force_refresh && self.snapshot.lock().initialized {
            return Ok(self.snapshot.lock().content.clone());
        }
        if !force_refresh {
            if let Some(rendered) = injection
                .last_injection
                .as_ref()
                .and_then(latest_system_reminder_content)
            {
                let content = frozen_session_start_content(rendered).map(str::to_string);
                self.record_session_start_snapshot(content.clone());
                return Ok(content);
            }
        }
        let content = self.render_session_start_reminder().await?;
        self.record_session_start_snapshot(content.clone());
        Ok(content)
    }

    fn record_session_start_snapshot(&self, content: Option<String>) {
        *self.snapshot.lock() = PluginSessionStartSnapshotState {
            initialized: true,
            content: content.clone(),
        };
        let dispatcher = self.dispatcher.clone();
        let event = PluginSessionStartEvent::new(PluginSessionStartEventPayload {
            agent_id: self.agent_id.clone(),
            content,
        });
        tokio::spawn(async move {
            let _ = dispatcher.dispatch(event.into()).await;
        });
    }
}

#[async_trait]
impl IAgentPluginService for AgentPluginService {
    async fn refresh_session_start(&self) -> Result<(), WireAsyncError> {
        if self.agent_id != MAIN_AGENT_ID {
            return Ok(());
        }
        self.refresh_pending.store(true, Ordering::Relaxed);
        self.skill_catalog.ready().await.map_err(PluginError)?;
        self.reminder
            .reconcile_when_idle(SESSION_START_INJECTION_VARIANT)
            .await;
        Ok(())
    }
}

fn render_plugin_change_reminder(mutation: &PluginMutation) -> String {
    let verb = match mutation.kind {
        PluginMutationKind::Install => "installed",
        PluginMutationKind::Enable => "enabled",
        PluginMutationKind::Disable => "disabled",
        PluginMutationKind::Remove => "removed",
        PluginMutationKind::McpServer => "updated",
    };
    format!(
        "Plugin \"{}\" was {verb}. This session keeps the prompt and tools it started with; run /new or /reload to apply the change, and tell the user if they expect it now.",
        mutation.id
    )
}

fn latest_system_reminder_content(message: &ContextMessage) -> Option<&str> {
    for part in &message.content {
        if let ContentPart::Text { text, .. } = part {
            if let Some(content) = system_reminder_content(text) {
                return Some(content);
            }
        }
    }
    None
}

fn frozen_session_start_content(rendered: &str) -> Option<&str> {
    if rendered == NO_ACTIVE_SESSION_STARTS {
        return None;
    }
    let suffix = format!("\n\n{SUPERSEDES_SUFFIX}");
    Some(rendered.strip_suffix(&suffix).unwrap_or(rendered))
}

fn render_plugin_session_start_reminder(
    session_starts: &[EnabledPluginSessionStart],
    catalog: &dyn SkillCatalog,
    log: Option<Arc<dyn ILogService>>,
    session_id: Option<&str>,
    warned_skills: &Mutex<HashSet<String>>,
) -> Option<String> {
    if session_starts.is_empty() {
        return None;
    }
    let mut blocks = Vec::new();
    for session_start in session_starts {
        let Some(skill) = catalog.get_plugin_skill(&session_start.plugin_id, &session_start.skill_name) else {
            let key = format!("{}:{}", session_start.plugin_id, session_start.skill_name);
            if warned_skills.lock().insert(key) {
                if let Some(log) = &log {
                    log.warn(
                        "plugin sessionStart skill not found",
                        Some(json!({
                            "pluginId": session_start.plugin_id,
                            "skillName": session_start.skill_name,
                        })),
                    );
                }
            }
            continue;
        };
        blocks.push(render_session_start_block(
            session_start,
            &skill,
            &catalog.render_skill_prompt(&skill, "", session_id),
        ));
    }
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

fn should_neutralize_plugin_session_start(history: &[ContextMessage]) -> bool {
    history.iter().any(|message| match &message.origin {
        Some(PromptOrigin::Injection(origin)) => {
            origin.variant == SESSION_START_INJECTION_VARIANT
        }
        Some(PromptOrigin::CompactionSummary(_)) => true,
        _ => false,
    })
}

fn render_session_start_block(
    session_start: &EnabledPluginSessionStart,
    skill: &SkillDefinition,
    skill_content: &str,
) -> String {
    format!(
        "<plugin_session_start plugin=\"{}\" skill=\"{}\">\n{skill_content}\n</plugin_session_start>",
        escape_xml_attr(&session_start.plugin_id),
        escape_xml_attr(&skill.name)
    )
}

#[derive(Debug)]
struct PluginError(String);

impl Display for PluginError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PluginError {}
