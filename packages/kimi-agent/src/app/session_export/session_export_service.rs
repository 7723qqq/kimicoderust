use crate::_base::di::instantiation::ServicesAccessorExt;
use crate::session::agent_lifecycle::agent_lifecycle::I_AGENT_LIFECYCLE_SERVICE;
use crate::_base::log::log::I_LOG_SERVICE;
use crate::session::session_metadata::session_metadata::I_SESSION_METADATA;
use crate::wire::wire::I_WIRE_SERVICE;
use std::io;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::{BoxFuture, FutureExt};
use kimi_core::primitives::errors::domains::session::SESSION_NOT_FOUND;
use kimi_core::primitives::errors::{Error2, Error2Options};
use serde_json::{Map, Value, json};

use crate::_base::di::scope::{ISessionScopeHandle, ScopeActivation, register_scoped_service};
use crate::_base::log::log::ILogService;
use crate::_base::log::log_config::resolve_global_log_path;
use crate::_base::utils::abort::{AbortError, AbortSignal};
use crate::_base::utils::paths::canonical_path;
use crate::app::bootstrap::bootstrap::IBootstrapService;
use crate::app::scopes::LifecycleScope;
use crate::app::session_index::session_index::{ISessionIndex, SessionSummary};
use crate::app::session_manager::session_manager::ISessionManager;
use crate::app::workspace::workspace::IWorkspaceService;
use crate::features::fileHistory::fileHistory::FILE_HISTORY_BLOB_PREFIX;
use crate::session::agentLifecycle::agentLifecycle::IAgentLifecycleService;
use crate::session::sessionMetadata::sessionMetadata::ISessionMetadata;
use crate::wire::record::WireAsyncError;
use crate::wire::wire::IWireService;
use crate::workspace::sessionLifecycle::internal::addressing::{
    session_dir_of, workspace_persistence_scope,
};

use super::errors::{SESSION_EXPORT_MISSING_VERSION, SESSION_EXPORT_NOT_FOUND};
use super::file_source::{ZipSource, open_zip_source};
use super::manifest::{ExportSessionManifestSummary, build_export_manifest};
use super::session_export::{
    ExportSessionManifest, ExportSessionOptions, ExportSessionPayload, ExportSessionResult,
    ISessionExportService,
};
use super::wire_scan::scan_session_wire;
use super::zip::{
    ExtraZipEntry, SessionZipEntry, WriteExportZipArgs, collect_files_recursive,
    write_export_zip,
};

const SESSION_LOG_REL: &str = "logs/kimi-code.log";
const GLOBAL_LOG_REL: &str = "logs/global/kimi-code.log";
const WEB_LOG_REL: &str = "logs/kimi-web.jsonl";
const DESKTOP_LOG_REL: &str = "logs/kimi-desktop.log";

pub struct SessionExportService {
    bootstrap: Arc<dyn IBootstrapService>,
    index: Arc<dyn ISessionIndex>,
    sessions: Arc<dyn ISessionManager>,
    workspaces: Arc<dyn IWorkspaceService>,
    log: Arc<dyn ILogService>,
}

impl SessionExportService {
    pub fn new(
        bootstrap: Arc<dyn IBootstrapService>,
        index: Arc<dyn ISessionIndex>,
        sessions: Arc<dyn ISessionManager>,
        workspaces: Arc<dyn IWorkspaceService>,
        log: Arc<dyn ILogService>,
    ) -> Self {
        Self {
            bootstrap,
            index,
            sessions,
            workspaces,
            log,
        }
    }

    async fn flush_live_session(
        &self,
        summary: SessionSummary,
    ) -> Result<ExportSessionDirectorySummary, WireAsyncError> {
        let workspace = self.workspaces.get(&summary.workspace_id).await?;
        let session_dir = session_dir_of(
            &self.bootstrap.home_dir(),
            &workspace_persistence_scope(
                &self.bootstrap.scope("sessions"),
                &summary.workspace_id,
            ),
            &summary.id,
        );
        let workspace_dir = workspace.map(|workspace| workspace.root);
        let mut export_summary = ExportSessionDirectorySummary {
            id: summary.id.clone(),
            title: summary.title,
            workspace_dir: workspace_dir.clone(),
            session_dir,
        };
        let Some(handle) = self.live_session(&summary.id) else {
            return Ok(export_summary);
        };

        let metadata_result = async {
            let metadata = handle.accessor().get(&I_SESSION_METADATA);
            metadata.ready().await?;
            metadata.read().await
        }
        .await;
        match metadata_result {
            Ok(meta) => {
                export_summary.id = meta.id;
                export_summary.title = meta.title;
            }
            Err(error) => {
                self.log.warn(
                    "flushMetadata failed before export",
                    Some(json!({ "error": error.to_string() })),
                );
            }
        }

        let session_log = handle.accessor().get(&I_LOG_SERVICE);
        self.warn_if_fails(
            "export session log flush failed",
            move || {
                let session_log = session_log.clone();
                async move {
                    session_log.flush().await;
                    Ok::<(), WireAsyncError>(())
                }
                .boxed()
            },
            false,
        )
        .await;
        let agents = handle.accessor().get(&I_AGENT_LIFECYCLE_SERVICE);
        for agent in agents.list(None) {
            let Some(agent_handle) = agents.handle_of(&agent.agent_id) else {
                continue;
            };
            let wire = agent_handle.accessor().get(&I_WIRE_SERVICE);
            self.warn_if_fails(
                "export agent wire flush failed",
                move || {
                    let wire = wire.clone();
                    async move { wire.flush().await }.boxed()
                },
                false,
            )
            .await;
        }
        Ok(export_summary)
    }

    fn live_session(&self, session_id: &str) -> Option<Arc<ISessionScopeHandle>> {
        self.sessions.get(session_id)
    }

    async fn warn_if_fails<F>(&self, message: &str, mut operation: F, retry: bool)
    where
        F: FnMut() -> BoxFuture<'static, Result<(), WireAsyncError>>,
    {
        match operation().await {
            Ok(()) => return,
            Err(error) => {
                self.log.warn(
                    message,
                    Some(json!({ "error": error.to_string() })),
                );
            }
        }
        if retry {
            let _ = operation().await;
        }
    }
}

#[async_trait]
impl ISessionExportService for SessionExportService {
    async fn export(
        &self,
        input: ExportSessionPayload,
        options: Option<ExportSessionOptions>,
    ) -> Result<ExportSessionResult, WireAsyncError> {
        let options = options.unwrap_or_default();
        throw_if_aborted(options.signal.as_ref())?;
        if input.version.trim().is_empty() {
            let mut details = Map::new();
            details.insert(
                "sessionId".to_string(),
                Value::String(input.session_id.clone()),
            );
            return Err(Box::new(Error2::with_options(
                SESSION_EXPORT_MISSING_VERSION,
                "Session export requires a host version.",
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )));
        }

        let Some(summary) = self.index.get(&input.session_id).await? else {
            let mut details = Map::new();
            details.insert(
                "sessionId".to_string(),
                Value::String(input.session_id.clone()),
            );
            return Err(Box::new(Error2::with_options(
                SESSION_NOT_FOUND,
                format!("Session \"{}\" does not exist", input.session_id),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )));
        };
        let live_summary = self.flush_live_session(summary).await?;
        throw_if_aborted(options.signal.as_ref())?;
        if input.include_global_log == Some(true) {
            let log = self.log.clone();
            self.warn_if_fails(
                "export global log flush failed",
                move || {
                    let log = log.clone();
                    async move {
                        log.flush().await;
                        Ok::<(), WireAsyncError>(())
                    }
                    .boxed()
                },
                true,
            )
            .await;
        }

        let home_dir = self.bootstrap.home_dir();
        let desktop_log_path = if input.include_desktop_log == Some(true) {
            Some(
                Path::new(&home_dir)
                    .join("logs")
                    .join("kimi-code-desktop.log")
                    .to_string_lossy()
                    .replace('\\', "/"),
            )
        } else {
            None
        };
        export_session_directory(ExportSessionDirectoryInput {
            request: input,
            summary: live_summary,
            global_log_path: Some(resolve_global_log_path(&home_dir)),
            desktop_log_path,
            web_log: options.web_log,
            signal: options.signal,
        })
        .await
    }
}

#[derive(Debug, Clone)]
pub struct ExportSessionDirectorySummary {
    pub id: String,
    pub title: Option<String>,
    pub workspace_dir: Option<String>,
    pub session_dir: String,
}

impl ExportSessionDirectorySummary {
    fn manifest_summary(&self) -> ExportSessionManifestSummary {
        ExportSessionManifestSummary {
            id: self.id.clone(),
            title: self.title.clone(),
            workspace_dir: self.workspace_dir.clone(),
        }
    }
}

pub struct ExportSessionDirectoryInput {
    pub request: ExportSessionPayload,
    pub summary: ExportSessionDirectorySummary,
    pub global_log_path: Option<String>,
    pub desktop_log_path: Option<String>,
    pub web_log: Option<String>,
    pub signal: Option<AbortSignal>,
}

pub async fn export_session_directory(
    input: ExportSessionDirectoryInput,
) -> Result<ExportSessionResult, WireAsyncError> {
    let mut session_log_source: Option<ZipSource> = None;
    let mut session_log_source_transferred = false;
    let mut global_source: Option<ZipSource> = None;
    let mut global_source_transferred = false;
    let mut desktop_source: Option<ZipSource> = None;
    let mut desktop_source_transferred = false;

    let result = async {
        throw_if_aborted(input.signal.as_ref())?;
        let session_dir = input.summary.session_dir.clone();
        let session_log_path = Path::new(&session_dir)
            .join(SESSION_LOG_REL)
            .to_string_lossy()
            .replace('\\', "/");
        session_log_source = open_optional_zip_source(&session_log_path, input.signal.as_ref()).await?;
        if input.request.include_global_log == Some(true)
            && let Some(path) = &input.global_log_path
        {
            global_source = open_optional_zip_source(path, input.signal.as_ref()).await?;
        }
        if let Some(path) = &input.desktop_log_path {
            desktop_source = open_optional_zip_source(path, input.signal.as_ref()).await?;
        }
        let session_files = collect_files_recursive(&session_dir).await?;
        if session_files.is_empty() && session_log_source.is_none() {
            let mut details = Map::new();
            details.insert(
                "sessionId".to_string(),
                Value::String(input.summary.id.clone()),
            );
            details.insert(
                "sessionDir".to_string(),
                Value::String(session_dir.clone()),
            );
            return Err(Box::new(Error2::with_options(
                SESSION_EXPORT_NOT_FOUND,
                format!(
                    "Session \"{}\" has no exportable directory at \"{}\"",
                    input.summary.id, session_dir
                ),
                Error2Options {
                    details: Some(details),
                    ..Default::default()
                },
            )) as WireAsyncError);
        }

        let session_scan = scan_session_wire(&session_dir, input.signal.as_ref()).await?;
        let stable_session_log = session_log_source.clone();
        let mut selected_session_files = session_files
            .into_iter()
            .filter(|file| {
                file != &session_log_path
                    && !file
                        .split(|character| character == '/' || character == '\\')
                        .any(|component| component == FILE_HISTORY_BLOB_PREFIX)
            })
            .map(SessionZipEntry::Path)
            .collect::<Vec<_>>();
        if let Some(source) = stable_session_log.clone() {
            selected_session_files.push(SessionZipEntry::Source {
                path: session_log_path.clone(),
                source,
            });
            selected_session_files.sort_by(|left, right| {
                session_zip_entry_path(left).cmp(session_zip_entry_path(right))
            });
        }
        let bundled_web_log = input.web_log.is_some();
        let now = Utc::now();
        let base_manifest = build_export_manifest(super::manifest::BuildExportManifestArgs {
            summary: input.summary.manifest_summary(),
            now,
            version: input.request.version.clone(),
            wire_protocol_version: None,
            session_scan,
            session_log_path: stable_session_log
                .as_ref()
                .map(|_| SESSION_LOG_REL.to_string()),
            global_log_path: None,
            web_log_path: bundled_web_log.then(|| WEB_LOG_REL.to_string()),
            desktop_version: input.request.desktop_version.clone(),
            install_source: input.request.install_source.clone(),
            shell_env: input.request.shell_env.clone(),
        });
        let output_path = input
            .request
            .output_path
            .as_deref()
            .map(canonical_path)
            .unwrap_or_else(|| canonical_path(&default_export_zip_name(&input.summary.id, now)));
        let mut extras = Vec::new();
        if let Some(web_log) = &input.web_log {
            extras.push(ExtraZipEntry::Data {
                data: web_log.as_bytes().to_vec(),
                target: WEB_LOG_REL.to_string(),
            });
        }
        if let Some(source) = global_source.clone() {
            extras.push(ExtraZipEntry::Source {
                source,
                target: GLOBAL_LOG_REL.to_string(),
            });
        }
        if let Some(source) = desktop_source.clone() {
            extras.push(ExtraZipEntry::Source {
                source,
                target: DESKTOP_LOG_REL.to_string(),
            });
        }
        let manifest = ExportSessionManifest {
            global_log_path: global_source
                .as_ref()
                .map(|_| GLOBAL_LOG_REL.to_string()),
            desktop_log_path: desktop_source
                .as_ref()
                .map(|_| DESKTOP_LOG_REL.to_string()),
            ..base_manifest
        };
        let writing = write_export_zip(WriteExportZipArgs {
            output_path: output_path.clone(),
            manifest: manifest.clone(),
            session_dir: session_dir.clone(),
            session_files: selected_session_files,
            extra_entries: Some(extras),
            signal: input.signal.clone(),
        });
        session_log_source_transferred = session_log_source.is_some();
        global_source_transferred = global_source.is_some();
        desktop_source_transferred = desktop_source.is_some();
        let entries = writing.await?;
        Ok(ExportSessionResult {
            zip_path: output_path,
            entries,
            session_dir,
            manifest,
        })
    }
    .await;

    if let Some(source) = session_log_source
        && !session_log_source_transferred
    {
        let _ = source.close().await;
    }
    if let Some(source) = global_source
        && !global_source_transferred
    {
        let _ = source.close().await;
    }
    if let Some(source) = desktop_source
        && !desktop_source_transferred
    {
        let _ = source.close().await;
    }
    result
}

fn default_export_zip_name(session_id: &str, now: chrono::DateTime<Utc>) -> String {
    let short_id = session_id.chars().take(8).collect::<String>();
    format!(
        "kimi-debug-{short_id}-{}.zip",
        now.format("%Y%m%d-%H%M%S")
    )
}

fn session_zip_entry_path(entry: &SessionZipEntry) -> &str {
    match entry {
        SessionZipEntry::Path(path) => path,
        SessionZipEntry::Source { path, .. } => path,
    }
}

async fn open_optional_zip_source(
    path: &str,
    signal: Option<&AbortSignal>,
) -> Result<Option<ZipSource>, WireAsyncError> {
    match open_zip_source(path, signal).await {
        Ok(source) => Ok(Some(source)),
        Err(error) => {
            throw_if_aborted(signal)?;
            if is_missing_path(error.as_ref()) {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

fn is_missing_path(error: &(dyn std::error::Error + Send + Sync)) -> bool {
    error
        .downcast_ref::<io::Error>()
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal
        && signal.aborted()
    {
        let message = signal
            .reason()
            .map(|reason| reason.to_string())
            .unwrap_or_else(|| "The operation was aborted.".to_string());
        return Err(Box::new(AbortError::new(message)));
    }
    Ok(())
}

pub fn register_session_export_service() {
    register_scoped_service(
        LifecycleScope::App,
        ScopeActivation::OnScopeCreated,
        "sessionExport",
    );
}
