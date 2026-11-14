use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures_util::{StreamExt, stream::BoxStream};
use parking_lot::Mutex;
use serde_json::{Map, Value};

use crate::human::eventStore::journal::{
    JournalRecord, JournalRef, StoreJournal, SyncStoreJournal,
};
use crate::human::store::types::{AppendInput, EntryLine, Payload, StoreError};
use crate::wire::human::{HUMAN_AGENT_DOMAIN, human_event_type, human_record_type};
use crate::wire::tree::tree::WireLine;
use crate::wire::wire::IWireService;

pub const ENGINE_JOURNAL_DOMAIN: &str = HUMAN_AGENT_DOMAIN;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn to_journal_record(
    line: WireLine,
    domain: &str,
    branch: &str,
    seq: usize,
) -> Option<JournalRecord> {
    let record_type = line.record.get("type")?.as_str()?;
    let type_ = human_event_type(record_type, domain)?;
    let ts = line
        .record
        .get("time")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let kind = line
        .record
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("event")
        .to_string();
    let mut data = line.record;
    data.insert("type".to_string(), Value::String(type_.clone()));
    Some(JournalRecord {
        branch: branch.to_string(),
        seq: seq as u64,
        ts,
        type_,
        kind,
        data: Value::Object(data),
    })
}

fn to_entry_line(input: &AppendInput, seq: usize) -> EntryLine {
    let data = input.data.clone().unwrap_or(Value::Null);
    let size = serde_json::to_string(&data)
        .map(|value| value.encode_utf16().count())
        .unwrap_or(0);
    EntryLine {
        kind: "entry".to_string(),
        seq: seq as u64,
        ts: now(),
        type_: input.type_.clone(),
        payload: Payload::Inline {
            kind: input.kind.clone(),
            size,
            data,
        },
    }
}

struct WireStoreJournal {
    wire: Arc<dyn IWireService>,
    domain: String,
    records: Mutex<Option<Vec<JournalRecord>>>,
}

impl WireStoreJournal {
    fn records(&self) -> Vec<JournalRecord> {
        let mut cached = self.records.lock();
        if cached.is_none() {
            let branch = self.wire.journal_ref().branch;
            let mut records = Vec::new();
            for line in self
                .wire
                .read_human_chain()
                .unwrap_or_else(|error| panic!("failed to read wire journal: {error}"))
            {
                if let Some(record) = to_journal_record(line, &self.domain, &branch, records.len()) {
                    records.push(record);
                }
            }
            *cached = Some(records);
        }
        cached.clone().unwrap_or_default()
    }

    fn push(&self, record: JournalRecord) {
        let _ = self.records();
        self.records
            .lock()
            .as_mut()
            .expect("wire journal records initialized")
            .push(record);
    }
}

#[async_trait]
impl StoreJournal for WireStoreJournal {
    fn ref_(&self) -> JournalRef {
        let reference = self.wire.journal_ref();
        JournalRef {
            tree: reference.tree,
            branch: reference.branch,
        }
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        let mut record = input
            .data
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_else(Map::new);
        record.insert(
            "type".to_string(),
            Value::String(human_record_type(&self.domain, &input.type_)),
        );
        record.insert("kind".to_string(), Value::String(input.kind.clone()));
        self.wire.append(record, None);
        let seq = self.records().len();
        let journal_record = JournalRecord {
            branch: self.wire.journal_ref().branch,
            seq: seq as u64,
            ts: now(),
            type_: input.type_.clone(),
            kind: input.kind.clone(),
            data: input.data.clone().unwrap_or(Value::Null),
        };
        self.push(journal_record);
        Ok(to_entry_line(&input, seq))
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        futures_util::stream::iter(self.records().into_iter().map(Ok)).boxed()
    }

    fn next_seq(&self) -> usize {
        self.records().len()
    }

    async fn settled(&self) -> Result<(), StoreError> {
        self.wire
            .settled()
            .await
            .map_err(|error| StoreError::new("wire", error.to_string()))
    }
}

#[async_trait]
impl SyncStoreJournal for WireStoreJournal {
    fn read_sync(&self) -> Vec<JournalRecord> {
        self.records()
    }
}

pub fn wire_store_journal(
    wire: Arc<dyn IWireService>,
    domain: impl Into<String>,
) -> Arc<dyn SyncStoreJournal> {
    Arc::new(WireStoreJournal {
        wire,
        domain: domain.into(),
        records: Mutex::new(None),
    })
}

struct SeededStoreJournal {
    base: Arc<dyn SyncStoreJournal>,
    seed: Vec<JournalRecord>,
    appended: AtomicUsize,
}

#[async_trait]
impl StoreJournal for SeededStoreJournal {
    fn ref_(&self) -> JournalRef {
        self.base.ref_()
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        let entry = self.base.append(input).await?;
        self.appended.fetch_add(1, Ordering::AcqRel);
        Ok(entry)
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        futures_util::stream::iter(self.seed.clone().into_iter().map(Ok)).boxed()
    }

    fn next_seq(&self) -> usize {
        self.seed.len() + self.appended.load(Ordering::Acquire)
    }

    async fn settled(&self) -> Result<(), StoreError> {
        self.base.settled().await
    }
}

#[async_trait]
impl SyncStoreJournal for SeededStoreJournal {
    fn read_sync(&self) -> Vec<JournalRecord> {
        self.seed.clone()
    }
}

pub fn seeded_store_journal(
    base: Arc<dyn SyncStoreJournal>,
    seed: Vec<JournalRecord>,
) -> Arc<dyn SyncStoreJournal> {
    Arc::new(SeededStoreJournal {
        base,
        seed,
        appended: AtomicUsize::new(0),
    })
}

struct AppendedStoreJournal {
    base: Arc<dyn SyncStoreJournal>,
    base_count: usize,
    trailing: Vec<JournalRecord>,
    appended: AtomicUsize,
}

#[async_trait]
impl StoreJournal for AppendedStoreJournal {
    fn ref_(&self) -> JournalRef {
        self.base.ref_()
    }

    async fn append(&self, input: AppendInput) -> Result<EntryLine, StoreError> {
        let entry = self.base.append(input).await?;
        self.appended.fetch_add(1, Ordering::AcqRel);
        Ok(entry)
    }

    fn read(&self) -> BoxStream<'static, Result<JournalRecord, StoreError>> {
        futures_util::stream::iter(self.read_sync().into_iter().map(Ok)).boxed()
    }

    fn next_seq(&self) -> usize {
        self.base_count + self.trailing.len() + self.appended.load(Ordering::Acquire)
    }

    async fn settled(&self) -> Result<(), StoreError> {
        self.base.settled().await
    }
}

#[async_trait]
impl SyncStoreJournal for AppendedStoreJournal {
    fn read_sync(&self) -> Vec<JournalRecord> {
        let mut records = self.base.read_sync();
        records.extend(self.trailing.clone());
        records
    }
}

pub fn appended_store_journal(
    base: Arc<dyn SyncStoreJournal>,
    seed: Vec<JournalRecord>,
) -> Arc<dyn SyncStoreJournal> {
    let base_count = base.next_seq();
    let branch = base.ref_().branch;
    let trailing = seed
        .into_iter()
        .enumerate()
        .map(|(index, mut record)| {
            record.branch = branch.clone();
            record.seq = (base_count + index) as u64;
            record
        })
        .collect();
    Arc::new(AppendedStoreJournal {
        base,
        base_count,
        trailing,
        appended: AtomicUsize::new(0),
    })
}
