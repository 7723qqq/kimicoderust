use std::sync::Arc;

use crate::human::store::backend::backend::StoreBackend;
use crate::human::store::types::{CorruptionReport, EntryLine};

#[derive(Clone)]
pub struct TreeContext {
    pub backend: StoreBackend,
    pub offload_threshold: usize,
    pub fsync: bool,
    pub notify_append: Arc<dyn Fn(&str, &str, &EntryLine) + Send + Sync>,
    pub notify_corruption: Arc<dyn Fn(&CorruptionReport) + Send + Sync>,
}
