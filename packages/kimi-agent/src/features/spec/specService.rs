use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::spec::{SPEC_ALL_FILES, SPEC_PROGRESS_FILE, SPEC_REQUIRED_FILES, SpecData, stage_for};
use super::specOps::{SpecRevision, SpecState};
use crate::tools::spec_mode::spec_dir;

pub struct AgentSpecService {
    workspace_root: PathBuf,
    agent_id: String,
}

impl AgentSpecService {
    pub fn new(workspace_root: impl Into<PathBuf>, agent_id: impl Into<String>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            agent_id: agent_id.into(),
        }
    }

    pub fn spec_dir_path_for(&self, id: &str) -> PathBuf {
        spec_dir(&self.workspace_root, id)
    }

    pub fn active_spec_dir(&self, state: &SpecState) -> Option<PathBuf> {
        if !state.active {
            return None;
        }
        state.id.as_deref().map(|id| self.spec_dir_path_for(id))
    }

    pub async fn clear(&self, state: &SpecState) {
        let Some(dir) = self.active_spec_dir(state) else {
            return;
        };
        for name in SPEC_ALL_FILES {
            let _ = tokio::fs::remove_file(dir.join(name)).await;
        }
    }

    pub async fn status(&self, state: &SpecState) -> Option<SpecData> {
        let id = state.id.as_deref()?;
        if !state.active {
            return None;
        }
        self.document_data(id).await
    }

    pub async fn document_data(&self, id: &str) -> Option<SpecData> {
        let dir = self.spec_dir_path_for(id);
        let mut files = BTreeMap::new();
        let mut missing = Vec::new();
        for name in SPEC_REQUIRED_FILES {
            let content = read_spec_file(&dir, name).await;
            if content.trim().is_empty() {
                missing.push(name.to_string());
            }
            files.insert(name.to_string(), content);
        }
        let progress = read_spec_file(&dir, SPEC_PROGRESS_FILE).await;
        Some(SpecData {
            id: id.to_string(),
            dir: dir.to_string_lossy().into_owned(),
            files,
            progress,
            complete: missing.is_empty(),
            stage: stage_for(&missing),
            missing,
        })
    }

    pub fn build_revision(
        &self,
        state: &SpecState,
        data: &SpecData,
    ) -> Option<SpecRevision> {
        if !state.active {
            return None;
        }
        let id = state.id.as_deref()?;
        let parts: Vec<String> = SPEC_REQUIRED_FILES
            .iter()
            .map(|name| {
                format!(
                    "# {name}\n\n{}",
                    data.files.get(*name).map(String::as_str).unwrap_or("")
                )
            })
            .collect();
        let bytes = parts.join("\n\n").into_bytes();
        let version = state
            .revision_count
            .as_ref()
            .and_then(|counts| counts.get(id))
            .copied()
            .unwrap_or(0)
            + 1;
        Some(SpecRevision {
            agent_id: self.agent_id.clone(),
            id: id.to_string(),
            version,
            key: format!("spec/{id}/v{version}.md"),
            sha256: sha256(&bytes),
            bytes: bytes.len(),
        })
    }
}

async fn read_spec_file(dir: &Path, name: &str) -> String {
    tokio::fs::read_to_string(dir.join(name))
        .await
        .unwrap_or_default()
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
