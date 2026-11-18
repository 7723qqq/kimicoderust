use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use indexmap::IndexMap;

use crate::_base::di::instantiation_service::InstantiationService;

use super::debug_graph::{DebugGraph, DebugGraphEdge, DebugGraphNode, IDebugGraphService};
use super::scope_tree::{scope_container_label, walk_scope_containers};

pub struct DebugGraphService {
    root: Arc<InstantiationService>,
}

impl DebugGraphService {
    pub fn new(root: Arc<InstantiationService>) -> Self {
        Self { root }
    }
}

impl IDebugGraphService for DebugGraphService {
    fn graph(&self) -> DebugGraph {
        let infos = walk_scope_containers(&self.root);
        let path_by_container = infos
            .iter()
            .map(|info| (Arc::as_ptr(&info.container) as usize, info.path.clone()))
            .collect::<HashMap<_, _>>();
        let mut nodes = IndexMap::<String, DebugGraphNode>::new();
        for info in &infos {
            let states = info
                .container
                .cascade()
                .units_snapshot()
                .into_iter()
                .map(|unit| (unit.token.clone(), unit))
                .collect::<HashMap<_, _>>();
            for (token, uid) in info.container.services_snapshot() {
                let id = node_id(&info.path, &token);
                nodes.insert(
                    id.clone(),
                    DebugGraphNode {
                        id,
                        token: token.clone(),
                        scope_path: info.path.clone(),
                        uid: Some(uid),
                        state: states.get(&token).map(|unit| unit.state),
                    },
                );
            }
        }
        let path_of = |scope: usize| {
            path_by_container
                .get(&scope)
                .cloned()
                .unwrap_or_else(|| format!("#{}", self.root.cascade_tree().seq_of(scope)))
        };
        let mut endpoint_node = |path: String, token: String| {
            let id = node_id(&path, &token);
            if !nodes.contains_key(&id) {
                nodes.insert(
                    id.clone(),
                    DebugGraphNode {
                        id: id.clone(),
                        token,
                        scope_path: path,
                        uid: None,
                        state: None,
                    },
                );
            }
            id
        };
        let mut edges = Vec::new();
        for edge in self.root.dependency_graph().lock().edges() {
            edges.push(DebugGraphEdge {
                from: endpoint_node(path_of(edge.consumer.scope), edge.consumer.token.to_string()),
                to: endpoint_node(path_of(edge.dependency.scope), edge.dependency.token.to_string()),
                kind: edge.kind,
            });
        }
        DebugGraph {
            nodes: nodes.into_values().collect(),
            edges,
        }
    }
}

fn node_id(scope_path: &str, token: &str) -> String {
    format!("{scope_path}::{token}")
}

#[allow(dead_code)]
fn _label(root: &InstantiationService) -> String {
    scope_container_label(root)
}
