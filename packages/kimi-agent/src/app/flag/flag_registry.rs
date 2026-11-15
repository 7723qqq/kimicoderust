use std::sync::{Arc, LazyLock};

use indexmap::IndexMap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::_base::di::lifecycle::{DisposableRef, IDisposable};
use crate::errors::BugIndicatingError;

use super::flag::IFlagService;

pub type FlagId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FlagSurface {
    Core,
    Tui,
    Both,
}

pub type FlagExposure = Arc<dyn Fn(&dyn IFlagService) -> bool + Send + Sync>;

#[derive(Clone)]
pub struct FlagDefinitionInput {
    pub id: FlagId,
    pub title: String,
    pub description: String,
    pub env: String,
    pub default: bool,
    pub surface: FlagSurface,
    pub is_exposed: Option<FlagExposure>,
}

static CONTRIBUTED_FLAGS: LazyLock<Mutex<IndexMap<usize, FlagDefinitionInput>>> =
    LazyLock::new(|| Mutex::new(IndexMap::new()));
static NEXT_CONTRIBUTION_ID: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

pub fn register_flag_definition(definition: FlagDefinitionInput) {
    let id = NEXT_CONTRIBUTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    CONTRIBUTED_FLAGS.lock().insert(id, definition);
}

pub fn get_contributed_flags() -> Vec<FlagDefinitionInput> {
    CONTRIBUTED_FLAGS.lock().values().cloned().collect()
}

pub trait IFlagRegistry: Send + Sync {
    fn register(
        &self,
        definition: FlagDefinitionInput,
    ) -> Result<DisposableRef, BugIndicatingError>;

    fn get(&self, id: &str) -> Option<FlagDefinitionInput>;

    fn list(&self) -> Vec<FlagDefinitionInput>;
}
