use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};

use async_trait::async_trait;
use futures_util::future::{FutureExt, Shared};
use indexmap::IndexMap;
use parking_lot::Mutex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::_base::errors::serialize::make_error_payload;
use crate::_base::utils::abort::{AbortSignal, abort_error, abortable};
use crate::agent::mcp::mcp::{
    IAgentMcpService, MCP_INITIAL_LOAD_WAIT_MS, McpResolvedServer,
};
use crate::agent::mcp::mcpDiscoveryOps::{
    MCP_DISCOVERY_KEY, MCP_TOOLS_DISCOVERED_CLASS, McpDiscoveryState, McpToolCollision,
    McpToolCollisionTarget, McpToolsDiscovered, McpToolsDiscoveredPayload,
};
use crate::agent::mcp::mcpEvents::{
    AgentErrorEvent, AgentErrorEventPayload, McpServerStatus, McpServerStatusEventPayload,
    McpServerStatusPayload, McpServerStatusValue, ToolListUpdated, ToolListUpdatedPayload,
    ToolListUpdatedReason,
};
use crate::agent::mcp::mcpFailureOps::{
    MCP_FAILURE_KEY, McpFailureState, McpServerFailed, McpServerFailedPayload,
    McpServerFailureResolution, McpServerFailureResolved, McpServerFailureResolvedPayload,
};
use crate::agent::mcp::output::McpOriginalsTarget;
use crate::agent::mcp::tools::auth::{CreateMcpAuthToolOptions, create_mcp_auth_tool};
use crate::agent::mcp::tools::mcp::{McpToolOptions, create_mcp_tool};
use crate::agent::media::session_media_store::ISessionMediaStore;
use crate::agent::profile::profile::AgentProfileService;
use crate::agent::tool_executor::tool_executor::ExecutableTool;
use crate::app::telemetry::telemetry::ITelemetryService;
use crate::human::llm::message::ToolDescription;
use crate::mcp_core::connection_manager::{
    McpConnectionView, McpServerEntry, McpServerStatus as ConnectionStatus, McpStatusListener,
};
use crate::mcp_core::errors::{MCP_OAUTH_FAILED, MCP_TOOL_NAME_COLLISION};
use crate::mcp_core::oauth::service::McpOAuthService;
use crate::mcp_core::tool_naming::qualify_mcp_tool_name;
use crate::mcp_core::types::{MCPClient, MCPToolDefinition};
use crate::rpc::types::BoxFuture;
use crate::session::mcp::session_mcp_handle::ISessionMcpHandle;
use crate::state::eventDispatcher::IEventDispatcher;
use crate::tool::tool_contract::{ToolDisclosure, ToolSource};
use crate::tools::registry::RegistrationStatus;
use crate::wire::record::WireAsyncError;
use kimi_core::primitives::errors::Error2;

pub static MCP_MCP_TOOLS_BY_SERVER_KEY: LazyLock<String> =
    LazyLock::new(|| "mcp.mcpToolsByServer".to_string());
pub static MCP_DISCOVERY_WRITES_READY_KEY: LazyLock<String> =
    LazyLock::new(|| "mcp.discoveryWritesReady".to_string());

pub struct RegisteredMcpTool {
    pub disposable: DisposableRef,
    pub status: RegistrationStatus,
}

pub trait AgentMcpToolRegistry: Send + Sync {
    fn register(
        &self,
        tool: Arc<dyn ExecutableTool>,
        source: ToolSource,
        disclosure: ToolDisclosure,
    ) -> RegisteredMcpTool;
}

#[derive(Clone)]
struct McpToolRegistration {
    disposable: DisposableRef,
    server_name: String,
}

type DiscoveryWork = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

struct AgentMcpServiceInner {
    mcp_handle: Arc<dyn ISessionMcpHandle>,
    registry: Arc<dyn AgentMcpToolRegistry>,
    dispatcher: Arc<dyn IEventDispatcher>,
    telemetry: Arc<dyn ITelemetryService>,
    agent_id: String,
    profile: Arc<dyn AgentProfileService>,
    attachment_store: Arc<dyn ISessionMediaStore>,
    originals: Option<Arc<dyn Fn() -> Option<McpOriginalsTarget> + Send + Sync>>,
    mcp_tools: Mutex<IndexMap<String, McpToolRegistration>>,
    pending_mcp_tools: Mutex<IndexMap<String, McpToolRegistration>>,
    mcp_tools_by_server: Mutex<IndexMap<String, Vec<String>>>,
    pending_discoveries: Mutex<Vec<DiscoveryWork>>,
    failed_server_episodes: Mutex<HashSet<String>>,
    discovery: Mutex<McpDiscoveryState>,
    failures: Mutex<McpFailureState>,
    registered: AtomicBool,
    discovery_writes_ready: AtomicBool,
    initial_load_wait_expired: AtomicBool,
    failure_write_tail: Mutex<Shared<BoxFuture<'static, ()>>>,
    dispatch_tail: Mutex<Shared<BoxFuture<'static, ()>>>,
    subscriptions: Mutex<Vec<DisposableRef>>,
}

#[derive(Clone)]
pub struct AgentMcpService {
    inner: Arc<AgentMcpServiceInner>,
}

impl AgentMcpService {
    pub fn new(
        mcp_handle: Arc<dyn ISessionMcpHandle>,
        registry: Arc<dyn AgentMcpToolRegistry>,
        dispatcher: Arc<dyn IEventDispatcher>,
        telemetry: Arc<dyn ITelemetryService>,
        agent_id: String,
        profile: Arc<dyn AgentProfileService>,
        attachment_store: Arc<dyn ISessionMediaStore>,
        originals: Option<Arc<dyn Fn() -> Option<McpOriginalsTarget> + Send + Sync>>,
    ) -> Self {
        let service = Self {
            inner: Arc::new(AgentMcpServiceInner {
                mcp_handle,
                registry,
                dispatcher,
                telemetry,
                agent_id,
                profile,
                attachment_store,
                originals,
                mcp_tools: Mutex::new(IndexMap::new()),
                pending_mcp_tools: Mutex::new(IndexMap::new()),
                mcp_tools_by_server: Mutex::new(IndexMap::new()),
                pending_discoveries: Mutex::new(Vec::new()),
                failed_server_episodes: Mutex::new(HashSet::new()),
                discovery: Mutex::new(McpDiscoveryState::default()),
                failures: Mutex::new(McpFailureState::default()),
                registered: AtomicBool::new(false),
                discovery_writes_ready: AtomicBool::new(false),
                initial_load_wait_expired: AtomicBool::new(false),
                failure_write_tail: Mutex::new(async {}.boxed().shared()),
                dispatch_tail: Mutex::new(async {}.boxed().shared()),
                subscriptions: Mutex::new(Vec::new()),
            }),
        };
        let _ = service.register();
        service.attach_mcp_tools();
        service
    }

    fn connection_manager(&self) -> Arc<dyn McpConnectionView> {
        self.inner.mcp_handle.connection_manager()
    }

    pub fn attach_mcp_tools(&self) {
        for entry in self.list() {
            self.handle_mcp_server_status_change(entry);
        }
        let service = self.clone();
        let subscription = self.on_status_change(Arc::new(move |entry| {
            service.handle_mcp_server_status_change(entry);
        }));
        self.inner.subscriptions.lock().push(subscription.into());
    }

    pub fn dispose(&self) {
        for subscription in self.inner.subscriptions.lock().drain(..) {
            subscription.dispose();
        }
        for registration in self.inner.mcp_tools.lock().values() {
            registration.disposable.dispose();
        }
        for registration in self.inner.pending_mcp_tools.lock().values() {
            registration.disposable.dispose();
        }
    }

    fn reconnect_for_tool_call(
        &self,
        server_name: String,
        stale_client: Arc<dyn MCPClient>,
        signal: Option<AbortSignal>,
    ) -> BoxFuture<'static, Result<Option<Arc<dyn MCPClient>>, WireAsyncError>> {
        let service = self.clone();
        let work = async move {
            service
                .join_healed_or_reconnect(&server_name, stale_client)
                .await
        };
        Box::pin(async move {
            if let Some(signal) = signal {
                match abortable(work, signal).await {
                    Ok(result) => result,
                    Err(error) => Err(Box::new(error) as WireAsyncError),
                }
            } else {
                work.await
            }
        })
    }

    async fn join_healed_or_reconnect(
        &self,
        server_name: &str,
        stale_client: Arc<dyn MCPClient>,
    ) -> Result<Option<Arc<dyn MCPClient>>, WireAsyncError> {
        if let Some(healed) = self.resolved(server_name).map(|resolved| resolved.client) {
            if !Arc::ptr_eq(&healed, &stale_client) {
                return Ok(Some(healed));
            }
        }
        self.connection_manager()
            .reconnect_and_join(server_name)
            .await?;
        Ok(self.resolved(server_name).and_then(|resolved| {
            (!Arc::ptr_eq(&resolved.client, &stale_client)).then_some(resolved.client)
        }))
    }

    fn handle_mcp_server_status_change(&self, entry: McpServerEntry) {
        if !self.inner.mcp_handle.is_baseline_server(&entry.name) {
            return;
        }
        self.dispatch_detached(
            McpServerStatus::new(McpServerStatusEventPayload {
                agent_id: self.inner.agent_id.clone(),
                server: McpServerStatusPayload {
                    name: entry.name.clone(),
                    transport: entry.transport,
                    status: status_value(entry.status),
                    tool_count: entry.tool_count,
                    error: entry.error.clone(),
                    next_retry_at: entry.next_retry_at,
                },
            })
            .into(),
        );
        match entry.status {
            ConnectionStatus::Connected => {
                self.resolve_server_failure(&entry, McpServerFailureResolution::Recovered);
                self.register_connected_mcp_server(&entry);
            }
            ConnectionStatus::NeedsAuth => {
                self.resolve_server_failure(&entry, McpServerFailureResolution::NeedsAuth);
                self.register_needs_auth_mcp_server(&entry);
            }
            ConnectionStatus::Failed => self.record_server_failure(&entry),
            ConnectionStatus::Pending => {}
            ConnectionStatus::Removed => {
                self.resolve_server_failure(&entry, McpServerFailureResolution::Removed);
            }
            ConnectionStatus::Disabled => {
                self.resolve_server_failure(&entry, McpServerFailureResolution::Disabled);
                if self.unregister_mcp_server(&entry.name) {
                    self.dispatch_tool_list_updated(
                        ToolListUpdatedReason::McpDisconnected,
                        &entry.name,
                    );
                }
            }
        }
    }

    fn record_server_failure(&self, entry: &McpServerEntry) {
        if !self.inner.failed_server_episodes.lock().insert(entry.name.clone()) {
            return;
        }
        let service = self.clone();
        let entry = entry.clone();
        self.enqueue_failure_write(Arc::new(move || {
            let service = service.clone();
            let entry = entry.clone();
            Box::pin(async move {
                if service.inner.failures.lock().open.contains(&entry.name) {
                    return;
                }
                service.dispatch_detached(
                    McpServerFailed::new(McpServerFailedPayload {
                        agent_id: service.inner.agent_id.clone(),
                        server_name: entry.name.clone(),
                        transport: entry.transport,
                        error: entry.error.clone(),
                        next_retry_at: entry.next_retry_at,
                    })
                    .into(),
                );
                service.inner.failures.lock().open.push(entry.name.clone());
                if let Some(next_retry_at) = entry.next_retry_at {
                    service
                        .inner
                        .failures
                        .lock()
                        .retry_at
                        .insert(entry.name.clone(), next_retry_at);
                }
            })
        }));
    }

    fn resolve_server_failure(&self, entry: &McpServerEntry, resolution: McpServerFailureResolution) {
        self.inner.failed_server_episodes.lock().remove(&entry.name);
        let service = self.clone();
        let entry = entry.clone();
        self.enqueue_failure_write(Arc::new(move || {
            let service = service.clone();
            let entry = entry.clone();
            Box::pin(async move {
                if !service.inner.failures.lock().open.contains(&entry.name) {
                    return;
                }
                service.dispatch_detached(
                    McpServerFailureResolved::new(McpServerFailureResolvedPayload {
                        agent_id: service.inner.agent_id.clone(),
                        server_name: entry.name.clone(),
                        transport: entry.transport,
                        resolution,
                    })
                    .into(),
                );
                let mut failures = service.inner.failures.lock();
                failures.open.retain(|name| name != &entry.name);
                failures.retry_at.remove(&entry.name);
            })
        }));
    }

    fn enqueue_failure_write(&self, work: DiscoveryWork) {
        let inner = self.inner.clone();
        let previous = inner.failure_write_tail.lock().clone();
        let run = async move {
            previous.await;
            if !inner.discovery_writes_ready.load(Ordering::Relaxed) {
                inner.pending_discoveries.lock().push(work);
                return;
            }
            work().await;
        }
        .boxed()
        .shared();
        *inner.failure_write_tail.lock() = run.clone();
        tokio::spawn(run);
    }

    pub async fn flush_pending_discoveries(&self) {
        let previous = self.inner.failure_write_tail.lock().clone();
        self.inner
            .discovery_writes_ready
            .store(true, Ordering::Relaxed);
        let pending = self.inner.pending_discoveries.lock().drain(..).collect::<Vec<_>>();
        let inner = self.inner.clone();
        let flush = async move {
            previous.await;
            for work in pending {
                work().await;
            }
        }
        .boxed()
        .shared();
        *inner.failure_write_tail.lock() = flush.clone();
        flush.await;
    }

    fn register_connected_mcp_server(&self, entry: &McpServerEntry) {
        let Some(resolved) = self.resolved(&entry.name) else {
            return;
        };
        let result = self.register_mcp_server(
            &entry.name,
            resolved.client,
            &resolved.tools,
            &resolved.enabled_names,
            resolved.deferred,
        );
        self.emit_mcp_tool_collisions(&entry.name, &result.collisions);
        self.record_discovery(
            &entry.name,
            &resolved.raw_tools,
            &resolved.enabled_names,
            &result.collisions,
        );
        self.dispatch_tool_list_updated(ToolListUpdatedReason::McpConnected, &entry.name);
    }

    fn register_needs_auth_mcp_server(&self, entry: &McpServerEntry) {
        self.unregister_mcp_server(&entry.name);
        let Some(oauth_service) = self.oauth_service() else {
            return;
        };
        let Some(server_url) = self.get_remote_server_url(&entry.name) else {
            return;
        };
        let service = self.clone();
        let server_name = entry.name.clone();
        let tool = create_mcp_auth_tool(CreateMcpAuthToolOptions {
            server_name: entry.name.clone(),
            server_url,
            oauth_service,
            reconnect: Arc::new(move |signal| {
                let service = service.clone();
                let server_name = server_name.clone();
                Box::pin(async move { service.reconnect(&server_name, signal).await })
            }),
            timeout_ms: None,
        });
        let tool_name = tool.name().to_string();
        let registration = self.inner.registry.register(
            tool,
            ToolSource::Mcp,
            if self.connection_manager().deferred_of(&entry.name) {
                ToolDisclosure::Deferred
            } else {
                ToolDisclosure::Inline
            },
        );
        self.inner.mcp_tools.insert(
            tool_name.clone(),
            McpToolRegistration {
                disposable: registration.disposable,
                server_name: entry.name.clone(),
            },
        );
        self.inner
            .mcp_tools_by_server
            .lock()
            .insert(entry.name.clone(), vec![tool_name]);
        self.dispatch_tool_list_updated(ToolListUpdatedReason::McpConnected, &entry.name);
    }

    fn register_mcp_server(
        &self,
        server_name: &str,
        client: Arc<dyn MCPClient>,
        tools: &[ToolDescription],
        enabled_tools: &HashSet<String>,
        deferred: bool,
    ) -> McpServerRegistrationResult {
        let mut qualified_names = Vec::new();
        let mut collisions = Vec::new();
        let mut seen_in_this_call = HashMap::<String, String>::new();
        for tool in tools {
            if !enabled_tools.contains(&tool.name) {
                continue;
            }
            let qualified = qualify_mcp_tool_name(server_name, &tool.name);
            if let Some(first) = seen_in_this_call.get(&qualified) {
                collisions.push(McpToolCollision {
                    qualified,
                    tool_name: tool.name.clone(),
                    collides_with: McpToolCollisionTarget::SameServer {
                        tool_name: first.clone(),
                    },
                });
                continue;
            }
            let existing_entry = self.inner.mcp_tools.lock().get(&qualified).cloned();
            let pending_entry = self.inner.pending_mcp_tools.lock().get(&qualified).cloned();
            if let Some(collision) = existing_entry.as_ref().or(pending_entry.as_ref()) {
                if collision.server_name != server_name {
                    collisions.push(McpToolCollision {
                        qualified,
                        tool_name: tool.name.clone(),
                        collides_with: McpToolCollisionTarget::OtherServer {
                            server_name: collision.server_name.clone(),
                        },
                    });
                    continue;
                }
            }
            seen_in_this_call.insert(qualified.clone(), tool.name.clone());
            let service = self.clone();
            let tool_server_name = server_name.to_string();
            let stale_client = client.clone();
            let manager = self.connection_manager();
            let unauthorized_manager = manager.clone();
            let unauthorized_server_name = server_name.to_string();
            let removed_server_name = server_name.to_string();
            let registration = self.inner.registry.register(
                create_mcp_tool(
                    &qualified,
                    tool.clone(),
                    client.clone(),
                    McpToolOptions {
                        server_name: Some(server_name.to_string()),
                        attachment_store: Some(self.inner.attachment_store.clone()),
                        originals: self.inner.originals.clone(),
                        telemetry: Some(self.inner.telemetry.clone()),
                        provider_type: Some({
                            let profile = self.inner.profile.clone();
                            Arc::new(move || profile.get_model_provider_type(None))
                        }),
                        reconnect: Some(Arc::new(move |signal| {
                            service.reconnect_for_tool_call(
                                tool_server_name.clone(),
                                stale_client.clone(),
                                signal,
                            )
                        })),
                        is_removed: Some(Arc::new(move || {
                            manager
                                .get(&removed_server_name)
                                .is_some_and(|entry| entry.status == ConnectionStatus::Removed)
                        })),
                        on_unauthorized: Some(Arc::new(move |error, failed_client| {
                            let manager = unauthorized_manager.clone();
                            let server_name = unauthorized_server_name.clone();
                            let error: WireAsyncError = Box::new(Error2::new(
                                MCP_OAUTH_FAILED,
                                error.to_string(),
                            ));
                            Box::pin(async move {
                                manager
                                    .mark_needs_auth(&server_name, error, Some(failed_client))
                                    .await
                                    .unwrap_or(false)
                            })
                        })),
                    },
                ),
                ToolSource::Mcp,
                if deferred {
                    ToolDisclosure::Deferred
                } else {
                    ToolDisclosure::Inline
                },
            );
            if registration.status != RegistrationStatus::Pending {
                if let Some(existing) = existing_entry {
                    existing.disposable.dispose();
                }
                if let Some(pending) = pending_entry {
                    pending.disposable.dispose();
                }
                self.inner.mcp_tools.insert(
                    qualified.clone(),
                    McpToolRegistration {
                        disposable: registration.disposable,
                        server_name: server_name.to_string(),
                    },
                );
                self.inner.pending_mcp_tools.lock().remove(&qualified);
            } else {
                if let Some(pending) = pending_entry {
                    pending.disposable.dispose();
                }
                self.inner.pending_mcp_tools.insert(
                    qualified.clone(),
                    McpToolRegistration {
                        disposable: registration.disposable,
                        server_name: server_name.to_string(),
                    },
                );
            }
            qualified_names.push(qualified);
        }
        let stale_active = self
            .inner
            .mcp_tools
            .lock()
            .iter()
            .filter(|(name, entry)| entry.server_name == server_name && !seen_in_this_call.contains_key(*name))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in stale_active {
            if let Some(entry) = self.inner.mcp_tools.lock().remove(&name) {
                entry.disposable.dispose();
            }
        }
        let stale_pending = self
            .inner
            .pending_mcp_tools
            .lock()
            .iter()
            .filter(|(name, entry)| entry.server_name == server_name && !seen_in_this_call.contains_key(*name))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in stale_pending {
            if let Some(entry) = self.inner.pending_mcp_tools.lock().remove(&name) {
                entry.disposable.dispose();
            }
        }
        self.inner
            .mcp_tools_by_server
            .lock()
            .insert(server_name.to_string(), qualified_names.clone());
        McpServerRegistrationResult {
            registered: qualified_names,
            collisions,
        }
    }

    fn unregister_mcp_server(&self, server_name: &str) -> bool {
        let names = self
            .inner
            .mcp_tools_by_server
            .lock()
            .remove(server_name);
        let Some(names) = names else {
            return false;
        };
        for name in names {
            if let Some(registration) = self.inner.mcp_tools.lock().remove(&name) {
                registration.disposable.dispose();
            }
            if let Some(registration) = self.inner.pending_mcp_tools.lock().remove(&name) {
                registration.disposable.dispose();
            }
        }
        true
    }

    fn record_discovery(
        &self,
        server_name: &str,
        raw_tools: &[MCPToolDefinition],
        enabled_names: &HashSet<String>,
        collisions: &[McpToolCollision],
    ) {
        let mut enabled_names = enabled_names.iter().cloned().collect::<Vec<_>>();
        enabled_names.sort();
        let hash_input = format!(
            "{{\"tools\":{},\"enabledNames\":{},\"collisions\":{}}}",
            serde_json::to_string(raw_tools).expect("MCP tools are serializable"),
            serde_json::to_string(&enabled_names).expect("enabled tool names are serializable"),
            serde_json::to_string(collisions).expect("MCP collisions are serializable"),
        );
        let hash = format!("{:x}", Sha256::digest(hash_input.as_bytes()));
        let key = format!("{server_name}\n{hash}");
        if self.inner.discovery_writes_ready.load(Ordering::Relaxed) {
            if self.inner.discovery.lock().seen.contains(&key) {
                return;
            }
            self.dispatch_detached(
                McpToolsDiscovered::new(McpToolsDiscoveredPayload {
                    agent_id: self.inner.agent_id.clone(),
                    server_name: server_name.to_string(),
                    hash,
                    tools: raw_tools.to_vec(),
                    enabled_names,
                    collisions: (!collisions.is_empty()).then_some(collisions.to_vec()),
                })
                .into(),
            );
            let mut discovery = self.inner.discovery.lock();
            discovery.seen.push(key);
            if discovery.seen.len() > 500 {
                let excess = discovery.seen.len() - 500;
                discovery.seen.drain(..excess);
            }
            return;
        }
        let service = self.clone();
        let server_name = server_name.to_string();
        let raw_tools = raw_tools.to_vec();
        let collisions = collisions.to_vec();
        let work: DiscoveryWork = Arc::new(move || {
            let service = service.clone();
            let server_name = server_name.clone();
            let raw_tools = raw_tools.clone();
            let enabled_names = enabled_names.clone();
            let collisions = collisions.clone();
            let hash = hash.clone();
            let key = key.clone();
            Box::pin(async move {
                if service.inner.discovery.lock().seen.contains(&key) {
                    return;
                }
                service.dispatch_detached(
                    McpToolsDiscovered::new(McpToolsDiscoveredPayload {
                        agent_id: service.inner.agent_id.clone(),
                        server_name,
                        hash,
                        tools: raw_tools,
                        enabled_names,
                        collisions: (!collisions.is_empty()).then_some(collisions),
                    })
                    .into(),
                );
                service.inner.discovery.lock().seen.push(key);
                let mut discovery = service.inner.discovery.lock();
                if discovery.seen.len() > 500 {
                    let excess = discovery.seen.len() - 500;
                    discovery.seen.drain(..excess);
                }
            })
        });
        self.inner.pending_discoveries.lock().push(work);
    }

    fn emit_mcp_tool_collisions(&self, server_name: &str, collisions: &[McpToolCollision]) {
        if collisions.is_empty() {
            return;
        }
        let summary = collisions
            .iter()
            .map(|collision| match &collision.collides_with {
                McpToolCollisionTarget::SameServer { tool_name } => format!(
                    "\"{}\" -> {} (collides with \"{}\" from the same server)",
                    collision.tool_name, collision.qualified, tool_name
                ),
                McpToolCollisionTarget::OtherServer { server_name: other } => format!(
                    "\"{}\" -> {} (collides with server \"{}\")",
                    collision.tool_name, collision.qualified, other
                ),
            })
            .collect::<Vec<_>>()
            .join("; ");
        let message = format!(
            "MCP server \"{server_name}\" registered {} tool name{} that collide with existing qualified names; the losing tools were dropped: {summary}",
            collisions.len(),
            if collisions.len() == 1 { "" } else { "s" }
        );
        let details = HashMap::from([
            ("serverName".to_string(), Value::String(server_name.to_string())),
            (
                "collisions".to_string(),
                serde_json::to_value(collisions).unwrap(),
            ),
        ]);
        self.dispatch_detached(
            AgentErrorEvent::new(AgentErrorEventPayload {
                error: make_error_payload(
                    MCP_TOOL_NAME_COLLISION.to_string(),
                    message,
                    Some(details),
                    None,
                ),
                agent_id: self.inner.agent_id.clone(),
            })
            .into(),
        );
    }

    fn dispatch_tool_list_updated(&self, reason: ToolListUpdatedReason, server_name: &str) {
        self.dispatch_detached(
            ToolListUpdated::new(ToolListUpdatedPayload {
                agent_id: self.inner.agent_id.clone(),
                reason,
                server_name: server_name.to_string(),
            })
            .into(),
        );
    }

    fn dispatch_detached(&self, event: crate::app::event::event2::Event2) {
        let dispatcher = self.inner.dispatcher.clone();
        let previous = self.inner.dispatch_tail.lock().clone();
        let run = async move {
            previous.await;
            let _ = dispatcher.dispatch(event).await;
        }
        .boxed()
        .shared();
        *self.inner.dispatch_tail.lock() = run.clone();
        tokio::spawn(run);
    }
}

struct McpServerRegistrationResult {
    registered: Vec<String>,
    collisions: Vec<McpToolCollision>,
}

#[async_trait]
impl IAgentMcpService for AgentMcpService {
    fn register(&self) -> Result<(), WireAsyncError> {
        if self.inner.registered.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        LazyLock::force(&MCP_DISCOVERY_KEY);
        LazyLock::force(&MCP_FAILURE_KEY);
        LazyLock::force(&MCP_TOOLS_DISCOVERED_CLASS);
        Ok(())
    }

    fn oauth_service(&self) -> Option<Arc<McpOAuthService>> {
        self.connection_manager().oauth_service()
    }

    async fn wait_for_initial_load(
        &self,
        signal: Option<AbortSignal>,
        budget_ms: Option<u64>,
    ) -> Result<(), WireAsyncError> {
        throw_if_aborted(signal.as_ref())?;
        if self.inner.initial_load_wait_expired.load(Ordering::Relaxed) {
            return Ok(());
        }
        let budget_ms = budget_ms.unwrap_or(MCP_INITIAL_LOAD_WAIT_MS);
        if budget_ms == 0 {
            self.inner
                .initial_load_wait_expired
                .store(true, Ordering::Relaxed);
            return Ok(());
        }
        let handle = self.inner.mcp_handle.clone();
        let ready = async move { handle.ready().await };
        let race = async move {
            tokio::select! {
                result = ready => result.map(|_| true),
                () = tokio::time::sleep(std::time::Duration::from_millis(budget_ms)) => Ok(false),
            }
        };
        let settled = if let Some(signal) = signal {
            match abortable(race, signal).await {
                Ok(result) => result?,
                Err(error) => return Err(Box::new(error) as WireAsyncError),
            }
        } else {
            race.await?
        };
        if settled {
            return Ok(());
        }
        self.inner
            .initial_load_wait_expired
            .store(true, Ordering::Relaxed);
        let entries = self.list();
        self.inner.telemetry.track2(
            "mcp_initial_load_wait",
            Some(serde_json::Map::from_iter([
                ("budget_ms".to_string(), Value::from(budget_ms)),
                (
                    "pending_count".to_string(),
                    Value::from(
                        entries
                            .iter()
                            .filter(|entry| entry.status == ConnectionStatus::Pending)
                            .count(),
                    ),
                ),
                ("total_count".to_string(), Value::from(entries.len())),
            ])),
        );
        Ok(())
    }

    fn initial_load_duration_ms(&self) -> u64 {
        self.connection_manager().initial_load_duration_ms()
    }

    fn list(&self) -> Vec<McpServerEntry> {
        self.connection_manager().list()
    }

    fn resolved(&self, name: &str) -> Option<McpResolvedServer> {
        self.connection_manager().resolved(name)
    }

    fn get_remote_server_url(&self, name: &str) -> Option<String> {
        self.connection_manager().get_remote_server_url(name)
    }

    async fn reconnect(
        &self,
        name: &str,
        signal: Option<AbortSignal>,
    ) -> Result<(), WireAsyncError> {
        throw_if_aborted(signal.as_ref())?;
        self.connection_manager().reconnect(name).await?;
        throw_if_aborted(signal.as_ref())
    }

    fn on_status_change(&self, listener: McpStatusListener) -> DisposableRef {
        self.connection_manager().on_status_change(listener)
    }
}

fn status_value(status: ConnectionStatus) -> McpServerStatusValue {
    match status {
        ConnectionStatus::Pending => McpServerStatusValue::Pending,
        ConnectionStatus::Connected => McpServerStatusValue::Connected,
        ConnectionStatus::Failed => McpServerStatusValue::Failed,
        ConnectionStatus::Disabled => McpServerStatusValue::Disabled,
        ConnectionStatus::NeedsAuth => McpServerStatusValue::NeedsAuth,
        ConnectionStatus::Removed => McpServerStatusValue::Removed,
    }
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal.filter(|signal| signal.aborted()) {
        let reason = signal.reason().unwrap_or_else(|| Arc::new(abort_error()));
        Err(Box::new(reason) as WireAsyncError)
    } else {
        Ok(())
    }
}
