use std::io;
use std::sync::Arc;

use async_trait::async_trait;
use kimi_core::primitives::errors::{Error2, unwrap_error_cause};
use once_cell::sync::Lazy;

use crate::_base::di::descriptors::{Constructor, DynamicValue, SyncDescriptor};
use crate::_base::di::instantiation::{
    DependencyKind, ScopeActivation, ServiceDependency, create_decorator,
};
use crate::_base::di::instantiation_service::register_descriptor_dependencies;
use crate::_base::di::scope::register_scoped_service;
use crate::_base::exec_env::decode_text::TextDecodeErrors;
use crate::app::scopes::LifecycleScope;
use crate::os::interface::host_file_system::{HostReadTextOptions, IHostFileSystem};
use crate::os::interface::host_fs_errors::{HostFsError, OS_FS_IS_DIRECTORY};
use crate::wire::record::WireAsyncError;

use super::edit_service::{EditApplyInput, EditApplyResult, EditService};
use super::file_edit::{FileEditInput, FileEditResult, I_FILE_EDIT_SERVICE, IFileEditService};
use super::text_model::TextModel;

pub struct FileEditService {
    fs: Arc<dyn IHostFileSystem>,
    editor: EditService,
}

impl FileEditService {
    pub fn new(fs: Arc<dyn IHostFileSystem>) -> Self {
        Self {
            fs,
            editor: EditService::new(),
        }
    }

    pub async fn edit(
        &self,
        input: FileEditInput,
        fs: Option<Arc<dyn IHostFileSystem>>,
    ) -> FileEditResult {
        let fs = fs.unwrap_or_else(|| self.fs.clone());
        let outcome: Result<FileEditResult, WireAsyncError> = async {
            let raw = fs
                .read_text(
                    &input.path,
                    Some(HostReadTextOptions {
                        errors: Some(TextDecodeErrors::Strict),
                        preserve_bom: Some(true),
                        ..Default::default()
                    }),
                )
                .await?;
            let model = TextModel::new(&raw);
            let result = self.editor.apply(
                &model,
                &EditApplyInput {
                    path: input.display_path.clone(),
                    old_string: input.old_string.clone(),
                    new_string: input.new_string.clone(),
                    replace_all: input.replace_all,
                },
            );
            match result {
                EditApplyResult::Err { error } => Ok(FileEditResult::Err { error }),
                EditApplyResult::Ok { raw_content, count } => {
                    fs.write_text(&input.path, &raw_content).await?;
                    Ok(FileEditResult::Ok { count })
                }
            }
        }
        .await;
        match outcome {
            Ok(result) => result,
            Err(error) if is_directory_error(&error) => FileEditResult::Err {
                error: format!("{} is not a file.", input.display_path),
            },
            Err(error) => FileEditResult::Err {
                error: error.to_string(),
            },
        }
    }
}

#[async_trait]
impl IFileEditService for FileEditService {
    async fn edit(
        &self,
        input: FileEditInput,
        fs: Option<Arc<dyn IHostFileSystem>>,
    ) -> FileEditResult {
        FileEditService::edit(self, input, fs).await
    }
}

fn is_directory_error(error: &WireAsyncError) -> bool {
    if error
        .downcast_ref::<HostFsError>()
        .is_some_and(|error| error.code().as_str() == OS_FS_IS_DIRECTORY)
    {
        return true;
    }
    let cause = unwrap_error_cause(error.as_ref());
    if cause
        .downcast_ref::<HostFsError>()
        .is_some_and(|error| error.code().as_str() == OS_FS_IS_DIRECTORY)
        || cause
            .downcast_ref::<Error2>()
            .is_some_and(|error| error.code().as_str() == "EISDIR")
    {
        return true;
    }
    cause.downcast_ref::<io::Error>().is_some_and(|error| {
        error.kind() == io::ErrorKind::IsADirectory || error.raw_os_error() == Some(21)
    })
}

fn dependency<T>(args: &[DynamicValue], index: usize) -> Arc<T>
where
    T: Send + Sync + 'static + ?Sized,
{
    Arc::downcast::<Arc<T>>(args[index].clone())
        .unwrap_or_else(|_| panic!("file edit dependency {index} has an incompatible type"))
        .as_ref()
        .clone()
}

static FILE_EDIT_SERVICE_REGISTRATION: Lazy<Result<(), String>> = Lazy::new(|| {
    let ctor: Constructor<DynamicValue> = Arc::new(|args| {
        let service: Arc<dyn IFileEditService> = Arc::new(FileEditService::new(dependency::<
            dyn IHostFileSystem,
        >(&args, 0)));
        Arc::new(service) as DynamicValue
    });
    let descriptor = SyncDescriptor::new(ctor, Vec::new());
    register_descriptor_dependencies(
        &descriptor,
        vec![ServiceDependency {
            id: create_decorator("hostFileSystem"),
            index: 0,
            kind: DependencyKind::Instance,
        }],
    );
    register_scoped_service(
        LifecycleScope::App.as_str().to_string(),
        I_FILE_EDIT_SERVICE.clone(),
        descriptor,
        ScopeActivation::OnScopeCreated,
        "edit",
    )
    .map_err(|error| error.to_string())
});

pub fn ensure_file_edit_service_registered() -> Result<(), String> {
    Lazy::force(&FILE_EDIT_SERVICE_REGISTRATION).clone()
}
