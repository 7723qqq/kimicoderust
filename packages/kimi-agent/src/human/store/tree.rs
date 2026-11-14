use std::collections::HashMap;
use std::sync::Arc;

use once_cell::sync::Lazy;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::Value;

use crate::human::store::branch::{Branch, BranchResolver};
use crate::human::store::internal::blob::read_blob;
use crate::human::store::internal::context::TreeContext;
use crate::human::store::types::{BranchRef, EntryLine, Payload, StoreError};

static BRANCH_NAME_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z0-9_][A-Za-z0-9._~-]*$").unwrap());

pub struct Tree {
    pub name: String,
    ctx: TreeContext,
    branch_map: Arc<Mutex<HashMap<String, Arc<Branch>>>>,
}

impl Tree {
    pub fn new(ctx: TreeContext, name: &str) -> Self {
        Self {
            name: name.to_string(),
            ctx,
            branch_map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn branches(&self) -> Vec<String> {
        let mut branches = self.branch_map.lock().keys().cloned().collect::<Vec<_>>();
        branches.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        branches
    }

    pub fn has(&self, name: &str) -> bool {
        self.branch_map.lock().contains_key(name)
    }

    pub fn open_branch(&self, name: &str) -> Result<Arc<Branch>, StoreError> {
        self.branch_map.lock().get(name).cloned().ok_or_else(|| {
            StoreError::new("unknown-branch", format!("unknown branch {}/{}", self.name, name))
        })
    }

    pub fn create_branch(
        &self,
        name: &str,
        from: Option<BranchRef>,
    ) -> Result<Arc<Branch>, StoreError> {
        if self.has(name) {
            return Err(StoreError::new(
                "duplicate-branch",
                format!("branch {}/{} already exists", self.name, name),
            ));
        }
        if !BRANCH_NAME_PATTERN.is_match(name) {
            return Err(StoreError::new(
                "invalid-name",
                format!("invalid branch name {name}"),
            ));
        }
        if let Some(parent_ref) = &from {
            let parent = self.open_branch(&parent_ref.branch)?;
            if parent_ref.seq >= parent.next_seq() as u64 {
                return Err(StoreError::new(
                    "invalid-target",
                    format!("cannot fork {} at {}", parent_ref.branch, parent_ref.seq),
                ));
            }
        }
        let resolver = self.resolver();
        let branch = Branch::create(self.ctx.clone(), &self.name, name, resolver, from);
        self.branch_map.lock().insert(name.to_string(), branch.clone());
        Ok(branch)
    }

    pub async fn load_branch(&self, name: &str) -> Result<Arc<Branch>, StoreError> {
        if self.has(name) {
            return Err(StoreError::new(
                "duplicate-branch",
                format!("branch {}/{} already exists", self.name, name),
            ));
        }
        let branch = Branch::load(self.ctx.clone(), &self.name, name, self.resolver()).await?;
        self.branch_map.lock().insert(name.to_string(), branch.clone());
        Ok(branch)
    }

    pub async fn resolve(&self, entry: &EntryLine) -> Result<Value, StoreError> {
        match &entry.payload {
            Payload::Offloaded { reference, .. } => {
                let data = read_blob(self.ctx.backend.blobs.as_ref(), reference).await?;
                serde_json::from_str(&data)
                    .map_err(|error| StoreError::new("syntax", error.to_string()))
            }
            Payload::Inline { data, .. } => Ok(data.clone()),
        }
    }

    fn resolver(&self) -> BranchResolver {
        let map = Arc::downgrade(&self.branch_map);
        Arc::new(move |name| {
            map.upgrade()
                .and_then(|map| map.lock().get(name).cloned())
        })
    }
}
