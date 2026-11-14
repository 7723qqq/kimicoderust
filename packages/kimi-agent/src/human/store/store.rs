use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;

use crate::human::store::backend::backend::StoreBackend;
use crate::human::store::internal::blob::read_blob;
use crate::human::store::internal::codec::{CodecErrorKind, parse_header, parse_line};
use crate::human::store::internal::context::TreeContext;
use crate::human::store::tree::Tree;
use crate::human::store::types::{
    CorruptionKind, CorruptionReport, Payload, Subscriber, SubscriberRegistration, TreeStoreOptions,
    DEFAULT_OFFLOAD_THRESHOLD, StoreError,
};

static TREE_NAME_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]*$").unwrap());

struct SubscriberEntry {
    id: u64,
    prefix: String,
    subscriber: Arc<dyn Subscriber>,
}

pub struct TreeStore {
    backend: StoreBackend,
    trees: Mutex<IndexMap<String, Arc<Tree>>>,
    subscribers: Arc<Mutex<Vec<SubscriberEntry>>>,
    next_subscriber: AtomicU64,
    ctx: TreeContext,
}

impl TreeStore {
    fn new(backend: StoreBackend, options: TreeStoreOptions) -> Self {
        let subscribers = Arc::new(Mutex::new(Vec::new()));
        let append_subscribers = subscribers.clone();
        let corruption_subscribers = subscribers.clone();
        let ctx = TreeContext {
            backend: backend.clone(),
            offload_threshold: options.offload_threshold.unwrap_or(DEFAULT_OFFLOAD_THRESHOLD),
            fsync: options.fsync.unwrap_or(false),
            notify_append: Arc::new(move |tree, branch, entry| {
                let subscribers = append_subscribers.lock();
                for registration in subscribers.iter() {
                    if entry.type_.starts_with(&registration.prefix) {
                        registration.subscriber.on_append(tree, branch, entry);
                    }
                }
            }),
            notify_corruption: Arc::new(move |report| {
                for registration in corruption_subscribers.lock().iter() {
                    registration.subscriber.on_corruption(report);
                }
            }),
        };
        {
            let mut target = subscribers.lock();
            for (index, SubscriberRegistration { prefix, subscriber }) in
                options.subscribers.into_iter().enumerate()
            {
                target.push(SubscriberEntry {
                    id: index as u64,
                    prefix,
                    subscriber,
                });
            }
        }
        Self {
            backend,
            trees: Mutex::new(IndexMap::new()),
            subscribers,
            next_subscriber: AtomicU64::new(0),
            ctx,
        }
    }

    pub async fn open(
        backend: StoreBackend,
        options: TreeStoreOptions,
    ) -> Result<Self, StoreError> {
        let initial_subscribers = options.subscribers.len();
        let store = Self::new(backend.clone(), options);
        store.next_subscriber.store(initial_subscribers as u64, Ordering::Release);
        for name in backend.trees.list().await? {
            let tree = Arc::new(Tree::new(store.ctx.clone(), &name));
            for branch_name in backend.trees.list_branches(&name).await? {
                tree.load_branch(&branch_name).await?;
            }
            store.trees.lock().insert(name, tree);
        }
        Ok(store)
    }

    pub fn names(&self) -> Vec<String> {
        self.trees.lock().keys().cloned().collect()
    }

    pub async fn tree(&self, name: &str) -> Result<Arc<Tree>, StoreError> {
        if let Some(existing) = self.trees.lock().get(name).cloned() {
            return Ok(existing);
        }
        if !TREE_NAME_PATTERN.is_match(name) {
            return Err(StoreError::new(
                "invalid-name",
                format!("invalid tree name {name}"),
            ));
        }
        let created = Arc::new(Tree::new(self.ctx.clone(), name));
        self.trees.lock().insert(name.to_string(), created.clone());
        Ok(created)
    }

    pub fn subscribe(
        &self,
        prefix: &str,
        subscriber: Arc<dyn Subscriber>,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let id = self.next_subscriber.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().push(SubscriberEntry {
            id,
            prefix: prefix.to_string(),
            subscriber,
        });
        let subscribers = self.subscribers.clone();
        Arc::new(move || {
            subscribers.lock().retain(|entry| entry.id != id);
        })
    }

    pub async fn verify(&self, blobs: bool) -> Result<Vec<CorruptionReport>, StoreError> {
        let mut reports = Vec::new();
        let trees = self
            .trees
            .lock()
            .iter()
            .map(|(name, tree)| (name.clone(), tree.clone()))
            .collect::<Vec<_>>();
        for (name, tree) in trees {
            for branch_name in tree.branches() {
                reports.extend(self.verify_branch(&name, &branch_name, &tree, blobs).await?);
            }
        }
        Ok(reports)
    }

    async fn verify_branch(
        &self,
        name: &str,
        branch_name: &str,
        tree: &Arc<Tree>,
        check_blobs: bool,
    ) -> Result<Vec<CorruptionReport>, StoreError> {
        let mut reports = Vec::new();
        let branch = tree.open_branch(branch_name)?;
        let content = match self.backend.trees.read(name, branch_name).await {
            Ok(content) => content,
            Err(error) => {
                reports.push(CorruptionReport {
                    tree: name.to_string(),
                    branch: branch_name.to_string(),
                    seq: None,
                    line: 0,
                    kind: CorruptionKind::Missing,
                    raw: None,
                    detail: error.to_string(),
                });
                return Ok(reports);
            }
        };
        let mut physical = content.split('\n').map(str::to_string).collect::<Vec<_>>();
        if physical.last().is_some_and(String::is_empty) {
            physical.pop();
        }
        let first = physical.first().cloned().unwrap_or_default();
        if let Err(error) = parse_header(&first) {
            reports.push(CorruptionReport {
                tree: name.to_string(),
                branch: branch_name.to_string(),
                seq: None,
                line: 1,
                kind: CorruptionKind::Header,
                raw: Some(first),
                detail: error.detail,
            });
        }
        let mut references = Vec::new();
        for index in 1..physical.len() {
            let raw = physical[index].clone();
            let expected_seq = (index - 1) as u64;
            match parse_line(&raw, expected_seq) {
                Ok(entry) => {
                    if let Payload::Offloaded { reference, .. } = entry.payload {
                        references.push(reference);
                    }
                }
                Err(error) => {
                    reports.push(CorruptionReport {
                        tree: name.to_string(),
                        branch: branch_name.to_string(),
                        seq: Some(expected_seq),
                        line: index + 1,
                        kind: match error.kind {
                            CodecErrorKind::Syntax => CorruptionKind::Syntax,
                            CodecErrorKind::Schema => CorruptionKind::Schema,
                            CodecErrorKind::Seq => CorruptionKind::SeqGap,
                        },
                        raw: Some(raw),
                        detail: error.detail,
                    });
                }
            }
        }
        if let Some(parent_branch) = branch.header.parent_branch.as_ref() {
            let parent = if tree.has(parent_branch) {
                Some(tree.open_branch(parent_branch)?)
            } else {
                None
            };
            let parent_exists = self
                .backend
                .trees
                .list_branches(name)
                .await?
                .contains(parent_branch);
            if parent.is_none() || !parent_exists {
                reports.push(CorruptionReport {
                    tree: name.to_string(),
                    branch: branch_name.to_string(),
                    seq: None,
                    line: 1,
                    kind: CorruptionKind::ParentRef,
                    raw: None,
                    detail: format!("parent branch {parent_branch} is missing"),
                });
            } else if let Some(parent_seq) = branch.header.parent_seq {
                let parent = parent.unwrap();
                if parent_seq >= parent.next_seq() as u64 {
                    reports.push(CorruptionReport {
                        tree: name.to_string(),
                        branch: branch_name.to_string(),
                        seq: None,
                        line: 1,
                        kind: CorruptionKind::ParentRef,
                        raw: None,
                        detail: format!("parent seq {parent_seq} is beyond {parent_branch}"),
                    });
                }
            }
        }
        if check_blobs {
            for reference in references {
                match read_blob(self.backend.blobs.as_ref(), &reference).await {
                    Ok(_) => {}
                    Err(error) if error.code == "blob-missing" => {
                        reports.push(blob_report(name, branch_name, CorruptionKind::BlobMissing, error));
                    }
                    Err(error) if error.code == "blob-crc" => {
                        reports.push(blob_report(name, branch_name, CorruptionKind::BlobCrc, error));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(reports)
    }
}

fn blob_report(
    tree: &str,
    branch: &str,
    kind: CorruptionKind,
    error: StoreError,
) -> CorruptionReport {
    CorruptionReport {
        tree: tree.to_string(),
        branch: branch.to_string(),
        seq: None,
        line: 0,
        kind,
        raw: None,
        detail: error.message,
    }
}
