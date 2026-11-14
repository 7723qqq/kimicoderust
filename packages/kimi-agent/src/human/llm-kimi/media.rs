use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use futures_util::future::FutureExt;
use parking_lot::Mutex;

use crate::human::llm::media::upload::ProviderMediaContribution;
use crate::human::llm::model::{LlmModel, model_key};
use crate::human::llm_kimi::files::{KimiFiles, KimiFilesFetch, KimiFilesOptions, KimiUploadOptions};
use crate::human::llm_kimi::r#trait::KIMI_DEFAULT_BASE_URL;

static FILES_BY_MODEL: LazyLock<Mutex<HashMap<String, Arc<KimiFiles>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Clone, Default)]
pub struct KimiMediaContributionOptions {
    pub fetch: Option<KimiFilesFetch>,
}

fn create_files(model: &LlmModel, media_fetch: Option<KimiFilesFetch>) -> Arc<KimiFiles> {
    Arc::new(KimiFiles::new(KimiFilesOptions {
        api_key: model.connection.api_key.clone(),
        base_url: model
            .connection
            .base_url
            .clone()
            .unwrap_or_else(|| KIMI_DEFAULT_BASE_URL.to_string()),
        default_headers: model.connection.default_headers.clone(),
        fetch: media_fetch,
    }))
}

fn resolve_files(model: &LlmModel, media_fetch: Option<KimiFilesFetch>) -> Arc<KimiFiles> {
    if media_fetch.is_some() {
        return create_files(model, media_fetch);
    }
    let key = model_key(model);
    if let Some(files) = FILES_BY_MODEL.lock().get(&key).cloned() {
        return files;
    }
    let files = create_files(model, None);
    FILES_BY_MODEL.lock().insert(key, files.clone());
    files
}

pub fn create_kimi_media_contribution(
    options: Option<KimiMediaContributionOptions>,
) -> ProviderMediaContribution {
    let fetch = options.unwrap_or_default().fetch;
    ProviderMediaContribution {
        inline_video: None,
        upload_video: Some(Arc::new(move |video, upload_options| {
            let files = resolve_files(&upload_options.model, fetch.clone());
            async move {
                files
                    .upload_video(
                        video,
                        Some(KimiUploadOptions {
                            signal: upload_options.signal,
                        }),
                    )
                    .await
            }
            .boxed()
        })),
        upload_image: None,
    }
}

pub static KIMI_MEDIA_CONTRIBUTION: LazyLock<ProviderMediaContribution> =
    LazyLock::new(|| create_kimi_media_contribution(None));
