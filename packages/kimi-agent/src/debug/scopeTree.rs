use std::sync::Arc;

use crate::_base::di::cascade_engine::CascadeEngine;
use crate::_base::di::instantiation_service::InstantiationService;

pub struct ScopeContainerInfo {
    pub container: Arc<InstantiationService>,
    pub path: String,
    pub label: String,
}

pub fn scope_container_label(container: &InstantiationService) -> String {
    container
        .debug_label
        .clone()
        .unwrap_or_else(|| format!("#{}", container.cascade_tree.seq_of(container)))
}

pub fn walk_scope_containers(root: &Arc<InstantiationService>) -> Vec<ScopeContainerInfo> {
    fn visit(container: &Arc<InstantiationService>, path: String, out: &mut Vec<ScopeContainerInfo>) {
        out.push(ScopeContainerInfo {
            container: container.clone(),
            path,
            label: scope_container_label(container),
        });
        for child in &container.children {
            visit(child, format!("{path}/{}", scope_container_label(child)), out);
        }
    }
    let mut out = Vec::new();
    visit(root, scope_container_label(root), &mut out);
    out
}

pub fn resolve_scope_container(
    root: &Arc<InstantiationService>,
    path: &str,
) -> Option<Arc<InstantiationService>> {
    walk_scope_containers(root)
        .into_iter()
        .find(|info| info.path == path)
        .map(|info| info.container)
}

pub fn scope_path_of_engine(
    root: &Arc<InstantiationService>,
    engine: &CascadeEngine,
) -> Option<String> {
    walk_scope_containers(root)
        .into_iter()
        .find(|info| std::ptr::eq(info.container.cascade.as_ref(), engine))
        .map(|info| info.path)
}
