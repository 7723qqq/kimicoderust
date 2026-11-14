use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex as StdMutex};
use std::time::Duration;

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::operationLog::{
    OperationGitHead, OperationLogError, OperationLogErrorCode, OperationLogErrorDetails,
    OperationLogErrorOperation, OperationView, OperationViewUnfinished,
};

const OBJECT_ID_PATTERN_LEN_MIN: usize = 40;
const MAX_SYMREF_DEPTH: usize = 8;
const FILE_CACHE_MAX_ENTRIES: usize = 50_000;
const BLOB_CACHE_MAX_BYTES: u64 = 32 * 1024 * 1024;
const BLOB_CACHE_MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024;
const CAPTURE_LOCK_RETRY_MS: u64 = 25;
const CAPTURE_LOCK_TIMEOUT_MS: u64 = 30_000;
const CAPTURE_LOCK_STALE_MS: u64 = 30_000;
const CAPTURE_LOCK_HEARTBEAT_MS: u64 = 10_000;
const GIT_TIMEOUT_MS: u64 = 60_000;

static OBJECT_LINE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9a-f]{40}|[0-9a-f]{64}) (blob|tree|commit|tag) (\d+)$")
        .expect("object line pattern compiles")
});

static CAPTURE_LOCKS: LazyLock<StdMutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

fn capture_lock_for(storage_root: &Path) -> Arc<Mutex<()>> {
    let mut locks = CAPTURE_LOCKS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    locks
        .entry(storage_root.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepositoryViewServiceOptions {
    pub storage_root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryFile {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryFileQuery {
    pub view: OperationView,
    pub path: String,
}

#[derive(Debug, Clone)]
struct PendingFile {
    tree: String,
    path: String,
    indexes: Vec<usize>,
}

#[derive(Debug, Clone)]
struct PendingBlob {
    hash: String,
    indexes: Vec<usize>,
}

#[derive(Debug, Default)]
struct BlobCache {
    entries: IndexMap<String, Vec<u8>>,
    bytes: u64,
}

pub struct RepositoryViewService {
    repo_root: PathBuf,
    git_dir: PathBuf,
    storage_root: PathBuf,
    actual_object_dir: PathBuf,
    shadow_index_path: PathBuf,
    shadow_object_dir: PathBuf,
    recovery_path: PathBuf,
    refs_from_files: bool,
    git_dir_identity: Option<String>,
    file_cache: StdMutex<IndexMap<String, Option<RepositoryFile>>>,
    blob_cache: StdMutex<BlobCache>,
}

impl RepositoryViewService {
    pub async fn open(
        repo_root: impl AsRef<Path>,
        options: RepositoryViewServiceOptions,
    ) -> Result<Self, OperationLogError> {
        let root = resolve_path(repo_root.as_ref());
        let output = run_git(
            &root,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--show-toplevel",
                "--absolute-git-dir",
                "--git-path",
                "objects",
            ],
            None,
        )
        .await?;
        let lines: Vec<&str> = output
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if lines.len() < 3 {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Invalid,
                "git rev-parse did not resolve the repository root, git directory, and object directory",
            ));
        }
        let root = PathBuf::from(lines[0]);
        let git_dir = PathBuf::from(lines[1]);
        let object_dir = PathBuf::from(lines[2]);
        let storage_root = match options.storage_root {
            Some(root) => resolve_path(&root),
            None => resolve_path(&git_dir.join("kimi-operation-log")),
        };
        let refs_from_files = !path_exists(&git_dir.join("commondir")).await?
            && !path_exists(&git_dir.join("reftable")).await?;
        let mut service = Self {
            repo_root: root,
            git_dir: git_dir.clone(),
            storage_root: storage_root.clone(),
            actual_object_dir: object_dir,
            shadow_index_path: storage_root.join("indexes").join(new_nonce()).join("index"),
            shadow_object_dir: storage_root.join("objects"),
            recovery_path: storage_root.join("recovery-required.json"),
            refs_from_files,
            git_dir_identity: None,
            file_cache: StdMutex::new(IndexMap::new()),
            blob_cache: StdMutex::new(BlobCache::default()),
        };
        service.git_dir_identity = read_directory_identity(&git_dir).await;
        Ok(service)
    }

    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    pub async fn is_repository_current(&self) -> bool {
        match self.git_dir_identity.as_deref() {
            Some(identity) => {
                read_directory_identity(&self.git_dir).await.as_deref() == Some(identity)
            }
            None => false,
        }
    }

    pub async fn capture_view(&self) -> Result<OperationView, OperationLogError> {
        self.assert_no_recovery_required().await?;
        self.with_capture_lock(|| async move { self.capture_current_view().await })
            .await
    }

    pub async fn changed_files(
        &self,
        before: &OperationView,
        after: &OperationView,
    ) -> Result<Vec<String>, OperationLogError> {
        let result = self
            .git(
                &["diff", "--name-only", "-z", &before.tree, &after.tree],
                true,
            )
            .await?;
        Ok(result
            .stdout
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect())
    }

    pub async fn restore_view(
        &self,
        expected: &OperationView,
        target: &OperationView,
    ) -> Result<(), OperationLogError> {
        self.with_capture_lock(|| async move {
            let current = self.capture_current_view().await?;
            self.assert_restorable(target).await?;
            if !current.equals(expected) {
                return Err(OperationLogError::new(
                    OperationLogErrorCode::ViewMismatch,
                    "The repository changed after the expected view was captured",
                ));
            }
            if let Some(parent) = self.recovery_path.parent() {
                create_directory(parent).await.map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot create the recovery marker directory: {error}"),
                    )
                })?;
            }
            let marker = encode(&RecoveryMarker {
                current: current.clone(),
                target: target.clone(),
            })?;
            let mut marker_file = create_new_options()
                .open(&self.recovery_path)
                .await
                .map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot write the recovery marker: {error}"),
                    )
                })?;
            marker_file
                .write_all(marker.as_bytes())
                .await
                .map_err(|error| {
                    OperationLogError::new(
                        OperationLogErrorCode::Invalid,
                        format!("cannot write the recovery marker: {error}"),
                    )
                })?;
            drop(marker_file);
            if let Err(error) = self.apply_view(&current, target).await {
                let rollback = async {
                    let interrupted = self.capture_current_view().await?;
                    self.apply_view(&interrupted, &current).await?;
                    match fs::remove_file(&self.recovery_path).await {
                        Ok(()) => Ok(()),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(error) => Err(git_io_error(error)),
                    }
                }
                .await;
                return match rollback {
                    Ok(()) => Err(error),
                    Err(rollback_error) => Err(OperationLogError::new(
                        OperationLogErrorCode::RecoveryRequired,
                        format!(
                            "Git view restoration failed ({}); rollback failed ({})",
                            message_of(&error),
                            message_of(&rollback_error)
                        ),
                    )),
                };
            }
            if let Err(error) = fs::remove_file(&self.recovery_path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(OperationLogError::new(
                    OperationLogErrorCode::RecoveryRequired,
                    format!(
                        "Git view was restored but its recovery marker could not be removed: {error}"
                    ),
                ));
            }
            Ok(())
        })
        .await
    }

    pub async fn assert_restorable(&self, view: &OperationView) -> Result<(), OperationLogError> {
        self.assert_no_recovery_required().await?;
        if let Some(operation) = read_unfinished_operation(&self.git_dir).await? {
            return Err(OperationLogError::with_details(
                OperationLogErrorCode::UnfinishedOperation,
                format!("Cannot restore while a {operation} is in progress"),
                OperationLogErrorDetails {
                    limit: None,
                    maximum: None,
                    operation: Some(match operation.as_str() {
                        "merge" => OperationLogErrorOperation::Merge,
                        _ => OperationLogErrorOperation::Rebase,
                    }),
                },
            ));
        }
        if !self.actual_index_is_clean().await? {
            return Err(OperationLogError::new(
                OperationLogErrorCode::StagedChanges,
                "Cannot restore with staged changes",
            ));
        }
        if let Some(unfinished) = view.unfinished_operation.as_ref() {
            let operation = unfinished_as_str(unfinished);
            return Err(OperationLogError::with_details(
                OperationLogErrorCode::UnfinishedOperation,
                format!("Cannot restore a view recorded during a {operation}"),
                OperationLogErrorDetails {
                    limit: None,
                    maximum: None,
                    operation: Some(match unfinished {
                        OperationViewUnfinished::Merge => OperationLogErrorOperation::Merge,
                        OperationViewUnfinished::Rebase => OperationLogErrorOperation::Rebase,
                    }),
                },
            ));
        }
        let expected_index_tree = self.tree_for_head(&view.head).await?;
        if view.index_tree.is_empty() || view.index_tree != expected_index_tree {
            return Err(OperationLogError::new(
                OperationLogErrorCode::StagedChanges,
                "The target view contains staged changes",
            ));
        }
        Ok(())
    }

    pub async fn dispose(&self) {
        if let Some(parent) = self.shadow_index_path.parent() {
            let _ = fs::remove_dir_all(parent).await;
        }
    }

    pub async fn read_files(
        &self,
        queries: &[RepositoryFileQuery],
    ) -> Result<Vec<Option<RepositoryFile>>, OperationLogError> {
        let mut reads: Vec<Option<RepositoryFile>> = vec![None; queries.len()];
        let mut pending: Vec<PendingFile> = Vec::new();
        let mut pending_by_key: HashMap<String, usize> = HashMap::new();
        for (index, query) in queries.iter().enumerate() {
            let key = format!("{}\0{}", query.view.tree, query.path);
            if let Some(cached) = self.cached_file(&key) {
                reads[index] = cached;
                continue;
            }
            if let Some(&slot) = pending_by_key.get(&key) {
                pending[slot].indexes.push(index);
                continue;
            }
            pending_by_key.insert(key, pending.len());
            pending.push(PendingFile {
                tree: query.view.tree.clone(),
                path: query.path.clone(),
                indexes: vec![index],
            });
        }
        let (batched, unbatched): (Vec<PendingFile>, Vec<PendingFile>) = pending
            .into_iter()
            .partition(|entry| !entry.path.contains(['\n', '\r']));
        if !batched.is_empty() {
            let files = self.read_files_from_git(&batched).await?;
            for (entry, file) in batched.into_iter().zip(files) {
                let key = format!("{}\0{}", entry.tree, entry.path);
                self.cache_file(&key, file.clone());
                for target in entry.indexes {
                    reads[target] = file.clone();
                }
            }
        }
        for entry in unbatched {
            let file = self.read_file_from_git(&entry.tree, &entry.path).await?;
            let key = format!("{}\0{}", entry.tree, entry.path);
            self.cache_file(&key, file.clone());
            for target in entry.indexes {
                reads[target] = file.clone();
            }
        }
        Ok(reads)
    }

    pub async fn read_file(
        &self,
        view: &OperationView,
        path: &str,
    ) -> Result<Option<RepositoryFile>, OperationLogError> {
        let query = RepositoryFileQuery {
            view: view.clone(),
            path: path.to_string(),
        };
        Ok(self
            .read_files(&[query])
            .await?
            .into_iter()
            .next()
            .flatten())
    }

    pub async fn read_blobs(
        &self,
        hashes: &[String],
    ) -> Result<Vec<Option<Vec<u8>>>, OperationLogError> {
        let mut reads: Vec<Option<Vec<u8>>> = vec![None; hashes.len()];
        let mut pending: Vec<PendingBlob> = Vec::new();
        let mut pending_by_hash: HashMap<String, usize> = HashMap::new();
        for (index, hash) in hashes.iter().enumerate() {
            if let Some(cached) = self.cached_blob(hash) {
                reads[index] = Some(cached);
                continue;
            }
            if let Some(&slot) = pending_by_hash.get(hash) {
                pending[slot].indexes.push(index);
                continue;
            }
            pending_by_hash.insert(hash.clone(), pending.len());
            pending.push(PendingBlob {
                hash: hash.clone(),
                indexes: vec![index],
            });
        }
        if pending.is_empty() {
            return Ok(reads);
        }
        let mut input = String::new();
        for entry in &pending {
            input.push_str(&entry.hash);
            input.push('\n');
        }
        let output = self
            .git_bytes_with_input(&["cat-file", "--batch"], &input, true)
            .await?;
        let mut offset = 0usize;
        for entry in &pending {
            let header_end = output[offset..]
                .iter()
                .position(|byte| *byte == 0x0a)
                .map(|position| position + offset)
                .ok_or_else(|| git_object_error(&entry.hash, "truncated output"))?;
            let header = String::from_utf8_lossy(&output[offset..header_end]).to_string();
            offset = header_end + 1;
            if header.ends_with(" missing") {
                return Err(git_object_error(&entry.hash, "object is missing"));
            }
            let Some(metadata) = OBJECT_LINE_PATTERN.captures(&header) else {
                return Err(git_object_error(&entry.hash, "not a blob"));
            };
            if &metadata[2] != "blob" {
                return Err(git_object_error(&entry.hash, "not a blob"));
            }
            let size: usize = metadata[3].parse().unwrap_or(0);
            let end = (offset + size).min(output.len());
            let bytes = output[offset..end].to_vec();
            offset += size + 1;
            self.cache_blob(&entry.hash, bytes.clone());
            for target in &entry.indexes {
                reads[*target] = Some(bytes.clone());
            }
        }
        Ok(reads)
    }

    pub async fn read_blob(&self, hash: &str) -> Result<Vec<u8>, OperationLogError> {
        let hashes = vec![hash.to_string()];
        let bytes = self.read_blobs(&hashes).await?;
        bytes
            .into_iter()
            .next()
            .flatten()
            .ok_or_else(|| git_object_error(hash, "object is missing"))
    }

    async fn capture_current_view(&self) -> Result<OperationView, OperationLogError> {
        if let Some(parent) = self.shadow_index_path.parent() {
            create_directory(parent).await.map_err(git_io_error)?;
        }
        create_directory(&self.shadow_object_dir)
            .await
            .map_err(git_io_error)?;
        let head = self.read_head().await?;
        let read_tree_args: Vec<String> = if matches!(head, OperationGitHead::Unborn { .. }) {
            vec!["read-tree".into(), "--empty".into()]
        } else {
            vec!["read-tree".into(), "HEAD".into()]
        };
        let read_tree_args: Vec<&str> = read_tree_args.iter().map(String::as_str).collect();
        self.git(&read_tree_args, true).await?;
        self.git(&["add", "-A", "--", "."], true).await?;
        let tree = self
            .git(&["write-tree"], true)
            .await?
            .stdout
            .trim()
            .to_string();
        let (refs, index_tree, unfinished_operation) = tokio::try_join!(
            self.read_refs(),
            self.read_actual_index_tree(),
            read_unfinished_operation(&self.git_dir)
        )?;
        let unfinished_operation = unfinished_operation.map(|operation| match operation.as_str() {
            "merge" => OperationViewUnfinished::Merge,
            _ => OperationViewUnfinished::Rebase,
        });
        Ok(OperationView {
            tree,
            head,
            refs,
            index_tree,
            unfinished_operation,
        })
    }

    async fn apply_view(
        &self,
        current: &OperationView,
        target: &OperationView,
    ) -> Result<(), OperationLogError> {
        if current.tree != target.tree {
            self.git(&["read-tree", &current.tree], true).await?;
            self.git(&["read-tree", "--reset", "-u", &target.tree], true)
                .await?;
        }
        if !current.head.equals(&target.head) {
            match target.head {
                OperationGitHead::Detached { ref target } => {
                    let old_target = match &current.head {
                        OperationGitHead::Unborn { .. } => "0".repeat(40),
                        OperationGitHead::Branch { target, .. } => target.clone(),
                        OperationGitHead::Detached { target } => target.clone(),
                    };
                    self.git(
                        &["update-ref", "--no-deref", "HEAD", target, &old_target],
                        false,
                    )
                    .await?;
                }
                OperationGitHead::Branch { ref ref_name, .. }
                | OperationGitHead::Unborn { ref ref_name } => {
                    self.git(&["symbolic-ref", "HEAD", ref_name], false).await?;
                }
            }
        }
        if !refs_equal(&current.refs, &target.refs) {
            self.update_refs(&current.refs, &target.refs).await?;
        }
        if current.index_tree != target.index_tree {
            let read_tree_args: Vec<String> =
                if matches!(target.head, OperationGitHead::Unborn { .. }) {
                    vec!["read-tree".into(), "--empty".into()]
                } else {
                    vec!["read-tree".into(), head_target(&target.head).to_string()]
                };
            let read_tree_args: Vec<&str> = read_tree_args.iter().map(String::as_str).collect();
            self.git(&read_tree_args, false).await?;
        }
        Ok(())
    }

    async fn update_refs(
        &self,
        current: &BTreeMap<String, String>,
        target: &BTreeMap<String, String>,
    ) -> Result<(), OperationLogError> {
        let missing = "0".repeat(40);
        let mut commands = Vec::new();
        let mut names: Vec<&String> = current.keys().collect();
        names.sort();
        for name in names {
            let old_target = &current[name];
            match target.get(name) {
                None => commands.push(format!("delete {name} {old_target}")),
                Some(new_target) if new_target != old_target => {
                    commands.push(format!("update {name} {new_target} {old_target}"));
                }
                Some(_) => {}
            }
        }
        let mut added: Vec<&String> = target.keys().collect();
        added.sort();
        for name in added {
            if !current.contains_key(name) {
                commands.push(format!("update {name} {} {missing}", target[name]));
            }
        }
        if commands.is_empty() {
            return Ok(());
        }
        let mut input = commands.join("\n");
        input.push('\n');
        self.git_with_input(&["update-ref", "--stdin"], &input, false)
            .await?;
        Ok(())
    }

    async fn assert_no_recovery_required(&self) -> Result<(), OperationLogError> {
        if path_exists(&self.recovery_path).await? {
            return Err(OperationLogError::new(
                OperationLogErrorCode::RecoveryRequired,
                "A previous Git view restoration requires recovery",
            ));
        }
        Ok(())
    }

    async fn read_head(&self) -> Result<OperationGitHead, OperationLogError> {
        if self.refs_from_files
            && let Some(head) = self.read_head_file().await?
        {
            return Ok(head);
        }
        self.read_head_with_git().await
    }

    async fn read_head_file(&self) -> Result<Option<OperationGitHead>, OperationLogError> {
        let Some(raw) = read_text_file(&self.git_dir.join("HEAD")).await? else {
            return Ok(None);
        };
        let value = raw.trim();
        if value.contains('\n') {
            return Ok(None);
        }
        if let Some(ref_name) = value.strip_prefix("ref: ") {
            let ref_name = ref_name.trim();
            if !is_valid_ref_name(ref_name) {
                return Ok(None);
            }
            return Ok(match self.resolve_ref_file(ref_name, 0).await? {
                RefResolution::Invalid => None,
                RefResolution::Missing => Some(OperationGitHead::Unborn {
                    ref_name: ref_name.to_string(),
                }),
                RefResolution::Resolved { target } => Some(OperationGitHead::Branch {
                    ref_name: ref_name.to_string(),
                    target,
                }),
            });
        }
        if is_object_id(value) {
            return Ok(Some(OperationGitHead::Detached {
                target: value.to_string(),
            }));
        }
        Ok(None)
    }

    async fn resolve_ref_file(
        &self,
        ref_name: &str,
        depth: usize,
    ) -> Result<RefResolution, OperationLogError> {
        if depth >= MAX_SYMREF_DEPTH || !is_valid_ref_name(ref_name) {
            return Ok(RefResolution::Invalid);
        }
        if let Some(raw) = read_text_file(&self.git_dir.join(ref_name)).await? {
            let value = raw.trim();
            if let Some(next) = value.strip_prefix("ref: ") {
                return Box::pin(self.resolve_ref_file(next.trim(), depth + 1)).await;
            }
            return Ok(if is_object_id(value) {
                RefResolution::Resolved {
                    target: value.to_string(),
                }
            } else {
                RefResolution::Invalid
            });
        }
        return Ok(match self.read_packed_refs().await? {
            PackedRefs::Missing => RefResolution::Missing,
            PackedRefs::Invalid => RefResolution::Invalid,
            PackedRefs::Present(packed) => match packed.get(ref_name) {
                Some(target) => RefResolution::Resolved {
                    target: target.clone(),
                },
                None => RefResolution::Missing,
            },
        });
    }

    async fn read_head_with_git(&self) -> Result<OperationGitHead, OperationLogError> {
        let (symbolic, target) = tokio::try_join!(
            self.git_allow(&["symbolic-ref", "-q", "HEAD"], true, &[0, 1]),
            self.git_allow(&["rev-parse", "--verify", "--quiet", "HEAD"], true, &[0, 1])
        )?;
        if symbolic.exit_code == Some(0) {
            let ref_name = symbolic.stdout.trim().to_string();
            if target.exit_code != Some(0) {
                return Ok(OperationGitHead::Unborn { ref_name });
            }
            return Ok(OperationGitHead::Branch {
                ref_name,
                target: target.stdout.trim().to_string(),
            });
        }
        if target.exit_code != Some(0) {
            return Err(OperationLogError::new(
                OperationLogErrorCode::Invalid,
                "Cannot resolve HEAD as a branch, detached commit, or unborn branch",
            ));
        }
        Ok(OperationGitHead::Detached {
            target: target.stdout.trim().to_string(),
        })
    }

    async fn read_refs(&self) -> Result<BTreeMap<String, String>, OperationLogError> {
        if self.refs_from_files
            && let Some(refs) = self.read_refs_from_files().await?
        {
            return Ok(refs);
        }
        self.read_refs_with_git().await
    }

    async fn read_refs_from_files(
        &self,
    ) -> Result<Option<BTreeMap<String, String>>, OperationLogError> {
        let mut raw: BTreeMap<String, String> = match self.read_packed_refs().await? {
            PackedRefs::Missing => Default::default(),
            PackedRefs::Invalid => return Ok(None),
            PackedRefs::Present(packed) => packed,
        };
        if !self
            .collect_loose_refs(&self.git_dir.join("refs"), "refs", &mut raw)
            .await?
        {
            return Ok(None);
        }
        for value in raw.values() {
            if !is_object_id(value) && !value.starts_with("ref: ") {
                return Ok(None);
            }
        }
        let mut names: Vec<&String> = raw.keys().collect();
        names.sort_by(|left, right| compare_ref_names(left, right));
        let mut refs = BTreeMap::new();
        for name in names {
            if let Some(target) = resolve_ref_chain(&raw, name) {
                refs.insert(name.clone(), target);
            }
        }
        Ok(Some(refs))
    }

    async fn collect_loose_refs(
        &self,
        root: &Path,
        root_prefix: &str,
        refs: &mut BTreeMap<String, String>,
    ) -> Result<bool, OperationLogError> {
        let mut stack = vec![(root.to_path_buf(), root_prefix.to_string())];
        while let Some((directory, prefix)) = stack.pop() {
            let mut entries = match fs::read_dir(&directory).await {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Ok(false),
            };
            loop {
                let entry = match entries.next_entry().await {
                    Ok(Some(entry)) => entry,
                    Ok(None) => break,
                    Err(_) => return Ok(false),
                };
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name.ends_with(".lock") {
                    continue;
                }
                let ref_name = format!("{prefix}/{name}");
                let path = directory.join(&name);
                let file_type = match entry.file_type().await {
                    Ok(file_type) => file_type,
                    Err(_) => return Ok(false),
                };
                if file_type.is_dir() {
                    stack.push((path, ref_name));
                    continue;
                }
                if !file_type.is_file() || !is_valid_ref_name(&ref_name) {
                    continue;
                }
                let Some(raw) = read_text_file(&path).await? else {
                    return Ok(false);
                };
                refs.insert(ref_name, raw.trim().to_string());
            }
        }
        Ok(true)
    }

    async fn read_packed_refs(&self) -> Result<PackedRefs, OperationLogError> {
        let Some(raw) = read_text_file(&self.git_dir.join("packed-refs")).await? else {
            return Ok(PackedRefs::Missing);
        };
        let mut refs = BTreeMap::new();
        for line in raw.split('\n') {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(peeled) = line.strip_prefix('^') {
                if !is_object_id(peeled.trim()) {
                    return Ok(PackedRefs::Invalid);
                }
                continue;
            }
            let separator = match line.find(' ') {
                Some(separator) if separator > 0 => separator,
                _ => return Ok(PackedRefs::Invalid),
            };
            let target = &line[..separator];
            let name = line[separator + 1..].trim();
            if !is_object_id(target) || !is_valid_ref_name(name) {
                return Ok(PackedRefs::Invalid);
            }
            refs.insert(name.to_string(), target.to_string());
        }
        Ok(PackedRefs::Present(refs))
    }

    async fn read_refs_with_git(&self) -> Result<BTreeMap<String, String>, OperationLogError> {
        let result = self
            .git(
                &["for-each-ref", "--format=%(refname)%00%(objectname)"],
                true,
            )
            .await?;
        let mut refs = BTreeMap::new();
        for line in result.stdout.split('\n') {
            if line.is_empty() {
                continue;
            }
            let separator = match line.find('\0') {
                Some(separator) if separator > 0 => separator,
                _ => {
                    return Err(OperationLogError::new(
                        OperationLogErrorCode::Corrupt,
                        "Unexpected git ref record",
                    ));
                }
            };
            refs.insert(
                line[..separator].to_string(),
                line[separator + 1..].trim().to_string(),
            );
        }
        Ok(refs)
    }

    async fn read_actual_index_tree(&self) -> Result<String, OperationLogError> {
        if !path_exists(&self.git_dir.join("index")).await? {
            return self.empty_tree().await;
        }
        match self.git(&["write-tree"], false).await {
            Ok(result) => Ok(result.stdout.trim().to_string()),
            Err(error) if error.code != OperationLogErrorCode::Corrupt => Err(error),
            Err(error) => {
                let unmerged = self
                    .git(
                        &["diff", "--cached", "--name-only", "--diff-filter=U"],
                        false,
                    )
                    .await?;
                if unmerged.stdout.trim().is_empty() {
                    return Err(error);
                }
                Ok(String::new())
            }
        }
    }

    async fn actual_index_is_clean(&self) -> Result<bool, OperationLogError> {
        let result = self
            .git_allow(&["diff", "--cached", "--quiet"], false, &[0, 1])
            .await?;
        Ok(result.exit_code == Some(0))
    }

    async fn tree_for_head(&self, head: &OperationGitHead) -> Result<String, OperationLogError> {
        if matches!(head, OperationGitHead::Unborn { .. }) {
            return self.empty_tree().await;
        }
        Ok(self
            .git(
                &["rev-parse", &format!("{}^{{tree}}", head_target(head))],
                false,
            )
            .await?
            .stdout
            .trim()
            .to_string())
    }

    async fn empty_tree(&self) -> Result<String, OperationLogError> {
        Ok(self
            .git_with_input(&["mktree"], "", false)
            .await?
            .stdout
            .trim()
            .to_string())
    }

    async fn git(&self, args: &[&str], shadow_index: bool) -> Result<GitOutput, OperationLogError> {
        run_git_env(
            &self.repo_root,
            args,
            &self.git_env(shadow_index),
            None,
            None,
        )
        .await
    }

    async fn git_allow(
        &self,
        args: &[&str],
        shadow_index: bool,
        allowed_exit_codes: &[i32],
    ) -> Result<GitOutput, OperationLogError> {
        run_git_env(
            &self.repo_root,
            args,
            &self.git_env(shadow_index),
            None,
            Some(allowed_exit_codes),
        )
        .await
    }

    async fn git_with_input(
        &self,
        args: &[&str],
        input: &str,
        shadow_index: bool,
    ) -> Result<GitOutput, OperationLogError> {
        run_git_env(
            &self.repo_root,
            args,
            &self.git_env(shadow_index),
            Some(input),
            None,
        )
        .await
    }

    async fn git_bytes_with_input(
        &self,
        args: &[&str],
        input: &str,
        shadow_index: bool,
    ) -> Result<Vec<u8>, OperationLogError> {
        run_git_bytes_env(
            &self.repo_root,
            args,
            &self.git_env(shadow_index),
            Some(input.as_bytes()),
        )
        .await
    }

    async fn read_files_from_git(
        &self,
        entries: &[PendingFile],
    ) -> Result<Vec<Option<RepositoryFile>>, OperationLogError> {
        let mut trees: Vec<String> = Vec::new();
        for entry in entries {
            if !trees.contains(&entry.tree) {
                trees.push(entry.tree.clone());
            }
        }
        for tree in &trees {
            if tree.is_empty() {
                return Err(git_ls_error(tree, "invalid tree"));
            }
        }
        let mut input = String::new();
        for tree in &trees {
            input.push_str(tree);
            input.push('\n');
        }
        for entry in entries {
            input.push_str(&entry.tree);
            input.push(':');
            input.push_str(&entry.path);
            input.push('\n');
        }
        let result = self
            .git_with_input(&["cat-file", "--batch-check"], &input, true)
            .await?;
        let lines: Vec<&str> = result.stdout.split('\n').collect();
        for (index, tree) in trees.iter().enumerate() {
            if lines
                .get(index)
                .is_some_and(|line| line.ends_with(" missing"))
            {
                return Err(git_ls_error(tree, "tree object is missing"));
            }
        }
        let mut reads: Vec<Option<RepositoryFile>> = Vec::with_capacity(entries.len());
        for index in 0..entries.len() {
            let line = lines.get(trees.len() + index).copied().unwrap_or("");
            let file = OBJECT_LINE_PATTERN.captures(line).and_then(|metadata| {
                if &metadata[2] != "blob" {
                    return None;
                }
                Some(RepositoryFile {
                    hash: metadata[1].to_string(),
                    size: metadata[3].parse().unwrap_or(0),
                })
            });
            reads.push(file);
        }
        Ok(reads)
    }

    async fn read_file_from_git(
        &self,
        tree: &str,
        path: &str,
    ) -> Result<Option<RepositoryFile>, OperationLogError> {
        let result = self.git(&["ls-tree", "-z", tree, "--", path], true).await?;
        if result.stdout.is_empty() {
            return Ok(None);
        }
        let separator = result
            .stdout
            .find('\t')
            .unwrap_or_else(|| result.stdout.len().saturating_sub(1));
        let metadata: Vec<&str> = result.stdout[..separator].split(' ').collect();
        let kind = metadata.get(1).copied().unwrap_or("");
        let hash = metadata.get(2).copied().unwrap_or("");
        if kind != "blob" || hash.is_empty() {
            return Ok(None);
        }
        let size = self.git(&["cat-file", "-s", hash], true).await?;
        Ok(Some(RepositoryFile {
            hash: hash.to_string(),
            size: size.stdout.trim().parse().unwrap_or(0),
        }))
    }

    fn cached_file(&self, key: &str) -> Option<Option<RepositoryFile>> {
        let mut cache = self
            .file_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cached = cache.get(key).cloned();
        if let Some(index) = cache.get_index_of(key) {
            let last = cache.len() - 1;
            if index != last {
                cache.move_index(index, last);
            }
        }
        cached
    }

    fn cache_file(&self, key: &str, file: Option<RepositoryFile>) {
        let mut cache = self
            .file_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len() >= FILE_CACHE_MAX_ENTRIES
            && let Some(oldest) = cache.keys().next().cloned()
        {
            cache.shift_remove(&oldest);
        }
        cache.insert(key.to_string(), file);
    }

    fn cached_blob(&self, hash: &str) -> Option<Vec<u8>> {
        let mut cache = self
            .blob_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cached = cache.entries.get(hash).cloned();
        if let Some(index) = cache.entries.get_index_of(hash) {
            let last = cache.entries.len() - 1;
            if index != last {
                cache.entries.move_index(index, last);
            }
        }
        cached
    }

    fn cache_blob(&self, hash: &str, bytes: Vec<u8>) {
        if bytes.len() as u64 > BLOB_CACHE_MAX_ENTRY_BYTES {
            return;
        }
        let mut cache = self
            .blob_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.entries.contains_key(hash) {
            return;
        }
        cache.bytes += bytes.len() as u64;
        cache.entries.insert(hash.to_string(), bytes);
        while cache.bytes > BLOB_CACHE_MAX_BYTES {
            let Some(oldest) = cache.entries.keys().next().cloned() else {
                break;
            };
            if let Some(evicted) = cache.entries.shift_remove(&oldest) {
                cache.bytes -= evicted.len() as u64;
            }
        }
    }

    fn git_env(&self, shadow_index: bool) -> BTreeMap<String, String> {
        let mut env = std::env::vars().collect::<BTreeMap<String, String>>();
        env.insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        env.insert(
            "GIT_OBJECT_DIRECTORY".into(),
            self.shadow_object_dir.display().to_string(),
        );
        env.insert(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES".into(),
            self.actual_object_dir.display().to_string(),
        );
        if shadow_index {
            env.insert(
                "GIT_INDEX_FILE".into(),
                self.shadow_index_path.display().to_string(),
            );
        }
        env
    }

    async fn with_capture_lock<T, F, Fut>(&self, run: F) -> Result<T, OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, OperationLogError>>,
    {
        let _local = capture_lock_for(&self.storage_root).lock_owned().await;
        self.with_capture_file_lock(run).await
    }

    async fn with_capture_file_lock<T, F, Fut>(&self, run: F) -> Result<T, OperationLogError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, OperationLogError>>,
    {
        create_directory(&self.storage_root)
            .await
            .map_err(|error| {
                OperationLogError::new(
                    OperationLogErrorCode::Invalid,
                    format!("cannot create the capture storage: {error}"),
                )
            })?;
        let lock_path = self.storage_root.join("capture.lock");
        let token = new_nonce();
        let deadline = std::time::Instant::now() + Duration::from_millis(CAPTURE_LOCK_TIMEOUT_MS);
        loop {
            match create_new_options().open(&lock_path).await {
                Ok(mut file) => {
                    file.write_all(token.as_bytes()).await.map_err(|error| {
                        OperationLogError::new(
                            OperationLogErrorCode::Invalid,
                            format!("cannot write the capture lock: {error}"),
                        )
                    })?;
                    break;
                }
                Err(error) => {
                    let kind = error.kind();
                    if !matches!(
                        kind,
                        std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::PermissionDenied
                    ) {
                        return Err(OperationLogError::new(
                            OperationLogErrorCode::Invalid,
                            format!("cannot create the capture lock: {error}"),
                        ));
                    }
                    if kind == std::io::ErrorKind::PermissionDenied
                        && !path_exists(&lock_path).await?
                    {
                        return Err(OperationLogError::new(
                            OperationLogErrorCode::Invalid,
                            format!("cannot create the capture lock: {error}"),
                        ));
                    }
                    let age = fs::metadata(&lock_path)
                        .await
                        .ok()
                        .and_then(|info| info.modified().ok())
                        .and_then(|modified| modified.elapsed().ok())
                        .map(|elapsed| elapsed.as_millis() as u64)
                        .unwrap_or(0);
                    if age > CAPTURE_LOCK_STALE_MS {
                        match fs::remove_file(&lock_path).await {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(error) => return Err(git_io_error(error)),
                        }
                        continue;
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(OperationLogError::new(
                            OperationLogErrorCode::Busy,
                            "Repository view capture is locked by another process",
                        ));
                    }
                    tokio::time::sleep(Duration::from_millis(CAPTURE_LOCK_RETRY_MS)).await;
                }
            }
        }

        let heartbeat_path = lock_path.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(Duration::from_millis(CAPTURE_LOCK_HEARTBEAT_MS));
            interval.tick().await;
            loop {
                interval.tick().await;
                let path = heartbeat_path.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&path) {
                        let _ = file.set_modified(std::time::SystemTime::now());
                    }
                })
                .await;
            }
        });

        let result = run().await;
        heartbeat.abort();
        let owner = fs::read_to_string(&lock_path).await.ok();
        if owner.as_deref() == Some(token.as_str()) {
            let _ = fs::remove_file(&lock_path).await;
        }
        result
    }
}

impl Drop for RepositoryViewService {
    fn drop(&mut self) {
        if let Some(parent) = self.shadow_index_path.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

fn unfinished_as_str(value: &OperationViewUnfinished) -> &'static str {
    match value {
        OperationViewUnfinished::Merge => "merge",
        OperationViewUnfinished::Rebase => "rebase",
    }
}

enum RefResolution {
    Resolved { target: String },
    Missing,
    Invalid,
}

enum PackedRefs {
    Missing,
    Invalid,
    Present(BTreeMap<String, String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct RecoveryMarker {
    current: OperationView,
    target: OperationView,
}

struct GitOutput {
    stdout: String,
    exit_code: Option<i32>,
}

fn head_target(head: &OperationGitHead) -> &str {
    match head {
        OperationGitHead::Branch { target, .. } => target,
        OperationGitHead::Detached { target } => target,
        OperationGitHead::Unborn { .. } => "",
    }
}

fn refs_equal(left: &BTreeMap<String, String>, right: &BTreeMap<String, String>) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(name, target)| right.get(name).is_some_and(|other| other == target))
}

fn resolve_ref_chain(refs: &BTreeMap<String, String>, name: &str) -> Option<String> {
    let mut current = name.to_string();
    for _ in 0..MAX_SYMREF_DEPTH {
        let target = refs.get(&current)?;
        if let Some(next) = target.strip_prefix("ref: ") {
            current = next.trim().to_string();
            continue;
        }
        return is_object_id(target).then(|| target.clone());
    }
    None
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), OBJECT_ID_PATTERN_LEN_MIN | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_valid_ref_name(name: &str) -> bool {
    if !name.starts_with("refs/")
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("//")
        || name.contains("..")
        || name.contains("@{")
    {
        return false;
    }
    if name.chars().any(|character| {
        let code = character as u32;
        code < 0x20
            || code == 0x7f
            || matches!(character, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\')
    }) {
        return false;
    }
    name.split('/').all(|component| {
        !component.is_empty() && !component.starts_with('.') && !component.ends_with(".lock")
    })
}

fn compare_ref_names(left: &str, right: &str) -> std::cmp::Ordering {
    left.as_bytes().cmp(right.as_bytes())
}

fn resolve_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn new_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn message_of(error: &OperationLogError) -> &str {
    &error.message
}

fn git_io_error(error: std::io::Error) -> OperationLogError {
    OperationLogError::new(
        OperationLogErrorCode::Invalid,
        format!("git view I/O failed: {error}"),
    )
}

fn git_object_error(hash: &str, message: &str) -> OperationLogError {
    OperationLogError::new(
        OperationLogErrorCode::Corrupt,
        format!("git cat-file blob {hash} failed: {message}"),
    )
}

fn git_ls_error(tree: &str, message: &str) -> OperationLogError {
    OperationLogError::new(
        OperationLogErrorCode::Corrupt,
        format!("git ls-tree {tree} failed: {message}"),
    )
}

fn encode<T: Serialize>(value: &T) -> Result<String, OperationLogError> {
    serde_json::to_string(value).map_err(|error| {
        OperationLogError::new(
            OperationLogErrorCode::Invalid,
            format!("cannot encode the recovery marker: {error}"),
        )
    })
}

async fn read_text_file(path: &Path) -> Result<Option<String>, OperationLogError> {
    match fs::read_to_string(path).await {
        Ok(raw) => Ok(Some(raw)),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::IsADirectory
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(git_io_error(error)),
    }
}

async fn create_directory(path: &Path) -> Result<(), std::io::Error> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path).await
}

fn create_new_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

async fn path_exists(path: &Path) -> Result<bool, OperationLogError> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(git_io_error(error)),
    }
}

async fn read_directory_identity(path: &Path) -> Option<String> {
    use std::hash::{Hash, Hasher};
    let handle = tokio::task::spawn_blocking({
        let path = path.to_path_buf();
        move || same_file::Handle::from_path(path)
    })
    .await
    .ok()?
    .ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    handle.hash(&mut hasher);
    Some(format!("{:016x}", hasher.finish()))
}

async fn read_unfinished_operation(git_dir: &Path) -> Result<Option<String>, OperationLogError> {
    if path_exists(&git_dir.join("rebase-merge")).await?
        || path_exists(&git_dir.join("rebase-apply")).await?
    {
        return Ok(Some("rebase".to_string()));
    }
    if path_exists(&git_dir.join("MERGE_HEAD")).await? {
        return Ok(Some("merge".to_string()));
    }
    Ok(None)
}

async fn run_git(
    cwd: &Path,
    args: &[&str],
    env: Option<&BTreeMap<String, String>>,
) -> Result<GitOutput, OperationLogError> {
    let default_env;
    let env = match env {
        Some(env) => env,
        None => {
            default_env = std::env::vars()
                .chain(std::iter::once((
                    "GIT_TERMINAL_PROMPT".to_string(),
                    "0".to_string(),
                )))
                .collect();
            &default_env
        }
    };
    run_git_env(cwd, args, env, None, None).await
}

async fn run_git_env(
    cwd: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    input: Option<&str>,
    allowed_exit_codes: Option<&[i32]>,
) -> Result<GitOutput, OperationLogError> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        OperationLogError::new(
            OperationLogErrorCode::Invalid,
            format!("cannot spawn git: {error}"),
        )
    })?;
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input.as_bytes()).await.map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot write to git stdin: {error}"),
            )
        })?;
    }
    let output = tokio::time::timeout(
        Duration::from_millis(GIT_TIMEOUT_MS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| OperationLogError::new(OperationLogErrorCode::Busy, "git command timed out"))?
    .map_err(|error| {
        OperationLogError::new(
            OperationLogErrorCode::Invalid,
            format!("git command failed: {error}"),
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code();
    let allowed = allowed_exit_codes.unwrap_or(&[0]);
    if !exit_code.is_some_and(|code| allowed.contains(&code)) {
        let detail = stderr.trim();
        return Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!(
                "git {} failed: {}",
                args.join(" "),
                if detail.is_empty() {
                    "unknown error"
                } else {
                    detail
                }
            ),
        ));
    }
    Ok(GitOutput { stdout, exit_code })
}

async fn run_git_bytes_env(
    cwd: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, OperationLogError> {
    let mut command = tokio::process::Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env)
        .stdin(if input.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        OperationLogError::new(
            OperationLogErrorCode::Invalid,
            format!("cannot spawn git: {error}"),
        )
    })?;
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(input).await.map_err(|error| {
            OperationLogError::new(
                OperationLogErrorCode::Invalid,
                format!("cannot write to git stdin: {error}"),
            )
        })?;
    }
    let output = tokio::time::timeout(
        Duration::from_millis(GIT_TIMEOUT_MS),
        child.wait_with_output(),
    )
    .await
    .map_err(|_| OperationLogError::new(OperationLogErrorCode::Busy, "git command timed out"))?
    .map_err(|error| {
        OperationLogError::new(
            OperationLogErrorCode::Invalid,
            format!("git command failed: {error}"),
        )
    })?;
    if output.status.code() != Some(0) {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(OperationLogError::new(
            OperationLogErrorCode::Corrupt,
            format!(
                "git {} failed: {}",
                args.join(" "),
                if stderr.is_empty() {
                    "unknown error"
                } else {
                    stderr
                }
            ),
        ));
    }
    Ok(output.stdout)
}
