use std::sync::Arc;

use crate::human::persist::v2::migrate::{
    V2_SESSION_TREE_NAME, is_v2_session_dir, migrate_v2_session,
};
use crate::human::session::stores::SessionStores;
use crate::human::store::backend::node::NodeBackend;
use crate::human::store::store::TreeStore;
use crate::human::store::tree::Tree;
use crate::human::store::types::TreeStoreOptions;
use crate::wire::record::WireAsyncError;

#[derive(Clone, Debug, Default)]
pub struct OpenSessionStoreOptions {
    pub tree_name: Option<String>,
    pub fsync: Option<bool>,
}

pub struct OpenedSessionStore {
    pub store: TreeStore,
    pub tree: Arc<Tree>,
    pub stores: SessionStores,
    pub migrated: bool,
}

pub async fn open_session_store(
    dir: &str,
    options: OpenSessionStoreOptions,
) -> Result<OpenedSessionStore, WireAsyncError> {
    let mut migrated = false;
    if is_v2_session_dir(dir).await {
        migrate_v2_session(dir).await?;
        migrated = true;
    }
    let backend = NodeBackend::new(dir);
    let backend = backend.store_backend();
    let store = TreeStore::open(
        backend.clone(),
        TreeStoreOptions {
            fsync: Some(options.fsync.unwrap_or(false)),
            ..TreeStoreOptions::default()
        },
    )
    .await?;
    let tree = store
        .tree(
            options
                .tree_name
                .as_deref()
                .unwrap_or(V2_SESSION_TREE_NAME),
        )
        .await?;
    let stores = SessionStores::new(tree.clone(), backend);
    Ok(OpenedSessionStore {
        store,
        tree,
        stores,
        migrated,
    })
}
