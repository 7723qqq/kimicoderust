use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::fs;

use crate::human::store::backend::backend::{BlobBackend, StoreBackend, TreeBackend};
use crate::human::store::types::StoreError;

fn store_error(error: io::Error) -> StoreError {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::PermissionDenied => "EACCES",
        io::ErrorKind::AlreadyExists => "EEXIST",
        _ => "IO",
    };
    StoreError::new(code, error.to_string())
}

struct NodeTreeBackend {
    dir: PathBuf,
}

impl NodeTreeBackend {
    fn dir_of(&self, tree: &str) -> PathBuf {
        self.dir.join(tree)
    }

    fn path(&self, tree: &str, branch: &str) -> PathBuf {
        self.dir_of(tree).join(format!("{branch}.jsonl"))
    }
}

#[async_trait]
impl TreeBackend for NodeTreeBackend {
    async fn list(&self) -> Result<Vec<String>, StoreError> {
        let mut entries = match fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(store_error(error)),
        };
        let mut names = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let file_type = entry.file_type().await.map_err(store_error)?;
                    if file_type.is_dir() {
                        names.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
                Ok(None) => break,
                Err(error) => return Err(store_error(error)),
            }
        }
        names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        Ok(names)
    }

    async fn list_branches(&self, tree: &str) -> Result<Vec<String>, StoreError> {
        let mut entries = match fs::read_dir(self.dir_of(tree)).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(store_error(error)),
        };
        let mut names = Vec::new();
        loop {
            match entries.next_entry().await {
                Ok(Some(entry)) => {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if let Some(branch) = name.strip_suffix(".jsonl") {
                        names.push(branch.to_string());
                    }
                }
                Ok(None) => break,
                Err(error) => return Err(store_error(error)),
            }
        }
        names.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        Ok(names)
    }

    async fn read(&self, tree: &str, branch: &str) -> Result<String, StoreError> {
        fs::read_to_string(self.path(tree, branch))
            .await
            .map_err(store_error)
    }

    async fn append(&self, tree: &str, branch: &str, data: &str) -> Result<(), StoreError> {
        fs::create_dir_all(self.dir_of(tree))
            .await
            .map_err(store_error)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(tree, branch))
            .await
            .map_err(store_error)?;
        use tokio::io::AsyncWriteExt;
        file.write_all(data.as_bytes()).await.map_err(store_error)
    }

    async fn write(&self, tree: &str, branch: &str, content: &str) -> Result<(), StoreError> {
        fs::create_dir_all(self.dir_of(tree))
            .await
            .map_err(store_error)?;
        let path = self.path(tree, branch);
        let temporary = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        fs::write(&temporary, content).await.map_err(store_error)?;
        fs::rename(temporary, path).await.map_err(store_error)
    }

    async fn sync(&self, tree: &str, branch: &str) -> Result<(), StoreError> {
        let file = fs::File::open(self.path(tree, branch))
            .await
            .map_err(store_error)?;
        file.sync_all().await.map_err(store_error)
    }
}

struct NodeBlobBackend {
    dir: PathBuf,
}

impl NodeBlobBackend {
    fn path(&self, reference: &str) -> PathBuf {
        self.dir.join(reference)
    }
}

#[async_trait]
impl BlobBackend for NodeBlobBackend {
    async fn has(&self, reference: &str) -> Result<bool, StoreError> {
        Ok(fs::metadata(self.path(reference)).await.is_ok())
    }

    async fn read(&self, reference: &str) -> Result<String, StoreError> {
        fs::read_to_string(self.path(reference))
            .await
            .map_err(store_error)
    }

    async fn write(&self, reference: &str, data: &str) -> Result<(), StoreError> {
        fs::create_dir_all(&self.dir).await.map_err(store_error)?;
        let path = self.path(reference);
        let temporary = PathBuf::from(format!("{}.tmp", path.to_string_lossy()));
        fs::write(&temporary, data).await.map_err(store_error)?;
        fs::rename(temporary, path).await.map_err(store_error)
    }
}

pub struct NodeBackend {
    pub trees: Arc<dyn TreeBackend>,
    pub blobs: Arc<dyn BlobBackend>,
}

impl NodeBackend {
    pub fn new(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            trees: Arc::new(NodeTreeBackend {
                dir: root.join("trees"),
            }),
            blobs: Arc::new(NodeBlobBackend {
                dir: root.join("blobs"),
            }),
        }
    }

    pub fn store_backend(&self) -> StoreBackend {
        self.into()
    }
}

impl From<&NodeBackend> for StoreBackend {
    fn from(backend: &NodeBackend) -> Self {
        Self {
            trees: backend.trees.clone(),
            blobs: backend.blobs.clone(),
        }
    }
}
