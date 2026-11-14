use std::sync::Arc;

use crate::_base::utils::abort::{AbortSignal, abort_error, abortable};
use crate::app::file::file_service::{GetResult, IFileService};
use crate::human::llm::message::ContentPart;
use crate::wire::record::WireAsyncError;

use super::media_ref::daemon_file_ref_from_part;
use super::session_media_store::{ISessionMediaStore, SessionMediaMaterializeInput};

pub struct PromptMediaIntakeDeps {
    pub files: Arc<dyn IFileService>,
    pub media_store: Arc<dyn ISessionMediaStore>,
    pub signal: Option<AbortSignal>,
}

pub async fn materialize_prompt_daemon_refs(
    content: &[ContentPart],
    deps: &PromptMediaIntakeDeps,
) -> Result<(), WireAsyncError> {
    for part in content {
        throw_if_aborted(deps.signal.as_ref())?;
        let Some(daemon_part) = daemon_file_ref_from_part(part) else {
            continue;
        };
        if let Err(error) = materialize_ref(deps, &daemon_part.ref_.file_id).await {
            throw_if_aborted(deps.signal.as_ref())?;
            drop(error);
        }
    }
    Ok(())
}

async fn materialize_ref(
    deps: &PromptMediaIntakeDeps,
    file_id: &str,
) -> Result<(), WireAsyncError> {
    let files = deps.files.clone();
    let id = file_id.to_string();
    let get = async move { files.get(&id).await };
    let file = if let Some(signal) = deps.signal.clone() {
        match abortable(get, signal).await {
            Ok(result) => result?,
            Err(error) => Err(Box::new(error) as WireAsyncError),
        }
    } else {
        get.await?
    };
    let result = materialize_file(deps, file_id, file).await;
    if let Err(error) = result {
        throw_if_aborted(deps.signal.as_ref())?;
        drop(error);
    }
    Ok(())
}

async fn materialize_file(
    deps: &PromptMediaIntakeDeps,
    file_id: &str,
    file: GetResult,
) -> Result<(), WireAsyncError> {
    let size = file.meta.size;
    let name = file.meta.name.clone();
    let mime_type = file.meta.media_type.clone();
    let stream = Arc::new(move || file.stream(None));
    deps.media_store
        .materialize(SessionMediaMaterializeInput {
            file_id: file_id.to_string(),
            size,
            name,
            mime_type,
            stream,
            signal: deps.signal.clone(),
        })
        .await?;
    Ok(())
}

fn throw_if_aborted(signal: Option<&AbortSignal>) -> Result<(), WireAsyncError> {
    if let Some(signal) = signal.filter(|signal| signal.aborted()) {
        let reason = signal.reason().unwrap_or_else(|| Arc::new(abort_error()));
        Err(Box::new(reason) as WireAsyncError)
    } else {
        Ok(())
    }
}
