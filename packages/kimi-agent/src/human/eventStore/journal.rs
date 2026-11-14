use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream::BoxStream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::human::store::branch::Branch;
use crate::human::store::tree::Tree;
use crate::human::store::types::{
    AppendInput, EntryLine, Payload, StoreError, is_offloaded_payload,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    pub branch: String,
    pub seq: u64,
    pub ts: u64,
    #[serde(rename = "type")]
    pub type_: String,
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRef {
    pub tree: String,
    pub branch: String,
}

#[async_trait]
pub trait StoreJournal: Send + Sync {
    fn ref_(&self) -> JournalRef;
    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError>;
    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>>;
    fn next_seq(&self) -> usize;
    async fn settled(&self) -> Result<(), StoreError>;
}

#[async_trait]
pub trait SyncStoreJournal: StoreJournal {
    fn read_sync(&self) -> Vec<JournalRecord>;
}

struct MemoryJournal {
    ref_: JournalRef,
    entries: Mutex<Vec<EntryLine>>,
}

impl MemoryJournal {
    fn records(&self) -> Vec<JournalRecord> {
        self.entries
            .lock()
            .iter()
            .map(|entry| JournalRecord {
                branch: self.ref_.branch.clone(),
                seq: entry.seq,
                ts: entry.ts,
                type_: entry.type_.clone(),
                kind: entry.payload.kind().to_string(),
                data: if is_offloaded_payload(&entry.payload) {
                    Value::Null
                } else {
                    match &entry.payload {
                        Payload::Inline { data, .. } => data.clone(),
                        Payload::Offloaded { .. } => unreachable!(),
                    }
                },
            })
            .collect()
    }
}

#[async_trait]
impl StoreJournal for MemoryJournal {
    fn ref_(&self) -> JournalRef {
        self.ref_.clone()
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        let data = input.data.unwrap_or(Value::Null);
        let mut entries = self.entries.lock();
        let entry = EntryLine {
            kind: "entry".to_string(),
            seq: entries.len() as u64,
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            type_: input.type_,
            payload: Payload::Inline {
                kind: input.kind,
                size: serde_json::to_string(&data)
                    .map_err(|error| StoreError::new("schema", error.to_string()))?
                    .encode_utf16()
                    .count(),
                data,
            },
        };
        entries.push(entry.clone());
        Ok(entry)
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        Box::pin(futures_util::stream::iter(
            self.records().into_iter().map(Ok),
        ))
    }

    fn next_seq(&self) -> usize {
        self.entries.lock().len()
    }

    async fn settled(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[async_trait]
impl SyncStoreJournal for MemoryJournal {
    fn read_sync(&self) -> Vec<JournalRecord> {
        self.records()
    }
}

pub fn memory_journal(ref_: Option<JournalRef>) -> Arc<dyn SyncStoreJournal> {
    Arc::new(MemoryJournal {
        ref_: ref_.unwrap_or(JournalRef {
            tree: "memory".to_string(),
            branch: "main".to_string(),
        }),
        entries: Mutex::new(Vec::new()),
    })
}

struct BranchJournal {
    branch: Arc<Branch>,
    tree: Arc<Tree>,
}

#[async_trait]
impl StoreJournal for BranchJournal {
    fn ref_(&self) -> JournalRef {
        JournalRef {
            tree: self.branch.tree.clone(),
            branch: self.branch.name.clone(),
        }
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        self.branch.append(input).await
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        let branch = self.branch.clone();
        let tree = self.tree.clone();
        let resolve_tree = tree.clone();
        Box::pin(
            futures_util::stream::once(async move { read_branch_chain(branch, tree) })
                .map_ok(move |entries| {
                    let tree = resolve_tree.clone();
                    futures_util::stream::iter(entries).then(move |(branch, entry)| {
                        let tree = tree.clone();
                        async move {
                            let data = tree.resolve(&entry).await?;
                            Ok(JournalRecord {
                                branch,
                                seq: entry.seq,
                                ts: entry.ts,
                                type_: entry.type_.clone(),
                                kind: entry.payload.kind().to_string(),
                                data,
                            })
                        }
                    })
                })
                .try_flatten(),
        )
    }

    fn next_seq(&self) -> usize {
        self.branch.next_seq()
    }

    async fn settled(&self) -> Result<(), StoreError> {
        self.branch.settled().await
    }
}

pub fn journal_from_branch(branch: Arc<Branch>, tree: Arc<Tree>) -> Arc<dyn StoreJournal> {
    Arc::new(BranchJournal { branch, tree })
}

fn read_branch_chain(
    branch: Arc<Branch>,
    tree: Arc<Tree>,
) -> Result<Vec<(String, EntryLine)>, StoreError> {
    let mut chain = Vec::new();
    let mut current = Some(branch);
    let mut upto = None;
    while let Some(branch) = current {
        let head = upto.or_else(|| branch.head().map(|head| head as u64));
        let mut entries = Vec::new();
        if let Some(head) = head {
            for seq in 0..=head {
                if let Some(entry) = branch.entry_at(seq as usize) {
                    entries.push(entry);
                }
            }
        }
        let name = branch.name.clone();
        let parent_branch = branch.header.parent_branch.clone();
        let parent_seq = branch.header.parent_seq;
        current = match (&parent_branch, parent_seq) {
            (Some(parent), Some(_)) if tree.has(parent) => Some(tree.open_branch(parent)?),
            _ => None,
        };
        upto = parent_seq;
        chain.push((name, entries));
    }
    chain.reverse();
    Ok(chain
        .into_iter()
        .flat_map(|(branch, entries)| {
            entries
                .into_iter()
                .map(move |entry| (branch.clone(), entry))
        })
        .collect())
}
