use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::future::{BoxFuture, FutureExt, Shared};
use parking_lot::Mutex;

use crate::human::store::internal::blob::write_blob;
use crate::human::store::internal::codec::{
    CodecErrorKind, encode_header, encode_line, parse_header, parse_line,
};
use crate::human::store::internal::context::TreeContext;
use crate::human::store::types::{
    AppendInput, BranchHeader, BranchRef, CorruptionKind, CorruptionReport, EntryLine, Payload,
    StoreError,
};

pub type BranchResolver = Arc<dyn Fn(&str) -> Option<Arc<Branch>> + Send + Sync>;

pub struct Branch {
    pub tree: String,
    pub name: String,
    pub header: BranchHeader,
    ctx: TreeContext,
    resolve_branch: BranchResolver,
    entries: Mutex<Vec<Option<EntryLine>>>,
    degraded_flag: AtomicBool,
    truncate_at: Mutex<Option<usize>>,
    operation: tokio::sync::Mutex<()>,
    header_write: Shared<BoxFuture<'static, Result<(), StoreError>>>,
}

impl Branch {
    pub fn create(
        ctx: TreeContext,
        tree: &str,
        name: &str,
        resolve_branch: BranchResolver,
        from: Option<BranchRef>,
    ) -> Arc<Self> {
        let header = BranchHeader {
            version: 1,
            tree: tree.to_string(),
            branch: name.to_string(),
            created_at: now(),
            parent_branch: from.as_ref().map(|parent| parent.branch.clone()),
            parent_seq: from.map(|parent| parent.seq),
        };
        let backend = ctx.backend.trees.clone();
        let tree_name = tree.to_string();
        let branch_name = name.to_string();
        let encoded = encode_header(&header);
        let header_write = async move {
            backend.write(&tree_name, &branch_name, &encoded).await
        }
        .boxed()
        .shared();
        let pending = header_write.clone();
        tokio::spawn(async move {
            let _ = pending.await;
        });
        Arc::new(Self {
            tree: tree.to_string(),
            name: name.to_string(),
            header,
            ctx,
            resolve_branch,
            entries: Mutex::new(Vec::new()),
            degraded_flag: AtomicBool::new(false),
            truncate_at: Mutex::new(None),
            operation: tokio::sync::Mutex::new(()),
            header_write,
        })
    }

    pub async fn load(
        ctx: TreeContext,
        tree: &str,
        name: &str,
        resolve_branch: BranchResolver,
    ) -> Result<Arc<Self>, StoreError> {
        let content = ctx.backend.trees.read(tree, name).await?;
        let mut physical = content.split('\n').map(str::to_string).collect::<Vec<_>>();
        if physical.last().is_some_and(String::is_empty) {
            physical.pop();
        }
        let mut entries = Vec::new();
        let mut header = BranchHeader {
            version: 1,
            tree: tree.to_string(),
            branch: name.to_string(),
            created_at: now(),
            parent_branch: None,
            parent_seq: None,
        };
        let mut degraded = false;
        let mut truncate_at = None;
        let report = |kind: CorruptionKind,
                      seq: Option<u64>,
                      line: usize,
                      detail: String,
                      raw: Option<String>| {
            (ctx.notify_corruption)(&CorruptionReport {
                tree: tree.to_string(),
                branch: name.to_string(),
                seq,
                line,
                kind,
                raw,
                detail,
            });
        };
        match physical.first() {
            None => {
                report(CorruptionKind::Header, None, 1, "file is empty".to_string(), None);
                degraded = true;
            }
            Some(first) => match parse_header(first) {
                Ok(parsed) => header = parsed,
                Err(cause) => {
                    report(
                        CorruptionKind::Header,
                        None,
                        1,
                        cause.detail,
                        Some(first.clone()),
                    );
                    degraded = true;
                }
            },
        }
        let start_index = usize::from(!physical.is_empty());
        let mut truncated = false;
        for index in start_index..physical.len() {
            let raw = physical[index].clone();
            let expected_seq = (index - start_index) as u64;
            match parse_line(&raw, expected_seq) {
                Ok(entry) => entries.push(Some(entry)),
                Err(cause) => {
                    if cause.kind == CodecErrorKind::Syntax && index == physical.len() - 1 {
                        publish(&ctx, tree, name, &physical[..index]).await?;
                        truncated = true;
                        break;
                    }
                    let kind = match cause.kind {
                        CodecErrorKind::Syntax => CorruptionKind::Syntax,
                        CodecErrorKind::Schema => CorruptionKind::Schema,
                        CodecErrorKind::Seq => CorruptionKind::SeqGap,
                    };
                    report(
                        kind,
                        Some(expected_seq),
                        index + 1,
                        cause.detail,
                        Some(raw),
                    );
                    if cause.kind == CodecErrorKind::Seq {
                        degraded = true;
                        truncate_at = Some(index);
                        break;
                    }
                    entries.push(None);
                }
            }
        }
        if !truncated && !content.is_empty() && !content.ends_with('\n') {
            ctx.backend.trees.append(tree, name, "\n").await?;
        }
        Ok(Arc::new(Self {
            tree: tree.to_string(),
            name: name.to_string(),
            header,
            ctx,
            resolve_branch,
            entries: Mutex::new(entries),
            degraded_flag: AtomicBool::new(degraded),
            truncate_at: Mutex::new(truncate_at),
            operation: tokio::sync::Mutex::new(()),
            header_write: futures_util::future::ready(Ok(())).boxed().shared(),
        }))
    }

    pub fn degraded(&self) -> bool {
        self.degraded_flag.load(Ordering::Acquire)
    }

    pub fn next_seq(&self) -> usize {
        self.entries.lock().len()
    }

    pub fn head(&self) -> Option<usize> {
        self.entries.lock().len().checked_sub(1)
    }

    pub async fn append(self: &Arc<Self>, input: AppendInput) -> Result<EntryLine, StoreError> {
        let _operation = self.operation.lock().await;
        self.await_header().await?;
        self.assert_writable()?;
        let seq = self.entries.lock().len() as u64;
        let data = input.data.unwrap_or(serde_json::Value::Null);
        let serialized = serde_json::to_string(&data).expect("serializing JSON cannot fail");
        let size = serialized.len();
        let payload = if size > self.ctx.offload_threshold {
            let reference = write_blob(self.ctx.backend.blobs.as_ref(), &serialized).await?;
            Payload::Offloaded {
                kind: input.kind,
                size,
                reference,
            }
        } else {
            Payload::Inline {
                kind: input.kind,
                size,
                data,
            }
        };
        let entry = EntryLine {
            kind: "entry".to_string(),
            seq,
            ts: now(),
            type_: input.type_,
            payload,
        };
        self.write_line(&entry).await?;
        self.entries.lock().push(Some(entry.clone()));
        (self.ctx.notify_append)(&self.tree, &self.name, &entry);
        Ok(entry)
    }

    pub async fn repair(&self) -> Result<(), StoreError> {
        let _operation = self.operation.lock().await;
        self.await_header().await?;
        if !self.degraded() {
            return Ok(());
        }
        let content = self.ctx.backend.trees.read(&self.tree, &self.name).await?;
        let mut physical = content.split('\n').map(str::to_string).collect::<Vec<_>>();
        if physical.last().is_some_and(String::is_empty) {
            physical.pop();
        }
        let end = self.truncate_at.lock().unwrap_or(physical.len());
        let mut kept = physical[..end].to_vec();
        if kept.is_empty() {
            kept.push(String::new());
        }
        kept[0] = encode_header(&self.header).trim_end_matches('\n').to_string();
        publish(&self.ctx, &self.tree, &self.name, &kept).await?;
        self.degraded_flag.store(false, Ordering::Release);
        *self.truncate_at.lock() = None;
        Ok(())
    }

    pub async fn settled(&self) -> Result<(), StoreError> {
        self.await_header().await?;
        let _operation = self.operation.lock().await;
        Ok(())
    }

    async fn await_header(&self) -> Result<(), StoreError> {
        self.header_write.clone().await.map_err(|error| {
            StoreError::new("io", format!("failed to write the branch header: {error}"))
        })
    }

    pub fn entry_at(&self, seq: usize) -> Option<EntryLine> {
        self.entries.lock().get(seq).cloned().flatten()
    }

    pub fn tip(&self) -> Option<EntryLine> {
        self.entries.lock().iter().rev().flatten().cloned().next()
    }

    pub fn walk(&self) -> Vec<EntryLine> {
        let mut entries = self.walk_own(self.entries.lock().len().checked_sub(1));
        self.walk_parent(&mut entries);
        entries
    }

    fn walk_own(&self, from: Option<usize>) -> Vec<EntryLine> {
        let Some(from) = from else {
            return Vec::new();
        };
        let entries = self.entries.lock();
        (0..=from)
            .rev()
            .filter_map(|index| entries.get(index).and_then(Option::as_ref).cloned())
            .collect()
    }

    fn walk_parent(&self, output: &mut Vec<EntryLine>) {
        let (Some(parent_branch), Some(parent_seq)) = (
            self.header.parent_branch.as_ref(),
            self.header.parent_seq,
        ) else {
            return;
        };
        let Some(parent) = (self.resolve_branch)(parent_branch) else {
            return;
        };
        output.extend(parent.walk_own(Some(parent_seq as usize)));
        parent.walk_parent(output);
    }

    async fn write_line(&self, entry: &EntryLine) -> Result<(), StoreError> {
        self.ctx
            .backend
            .trees
            .append(&self.tree, &self.name, &encode_line(entry))
            .await?;
        if self.ctx.fsync {
            self.ctx.backend.trees.sync(&self.tree, &self.name).await?;
        }
        Ok(())
    }

    fn assert_writable(&self) -> Result<(), StoreError> {
        if self.degraded() {
            return Err(StoreError::new(
                "degraded",
                format!(
                    "branch {}/{} is degraded; call repair() first",
                    self.tree, self.name
                ),
            ));
        }
        Ok(())
    }
}

async fn publish(
    ctx: &TreeContext,
    tree: &str,
    name: &str,
    lines: &[String],
) -> Result<(), StoreError> {
    ctx.backend
        .trees
        .write(tree, name, &format!("{}\n", lines.join("\n")))
        .await
}

fn now() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}
