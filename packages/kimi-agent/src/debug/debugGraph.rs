use crate::_base::di::cascade_engine::UnitState;
use crate::_base::di::dependency_graph::DependencyEdgeKind;

#[derive(Clone)]
pub struct DebugGraphNode {
    pub id: String,
    pub token: String,
    pub scope_path: String,
    pub uid: Option<u64>,
    pub state: Option<UnitState>,
}

#[derive(Clone)]
pub struct DebugGraphEdge {
    pub from: String,
    pub to: String,
    pub kind: DependencyEdgeKind,
}

#[derive(Clone)]
pub struct DebugGraph {
    pub nodes: Vec<DebugGraphNode>,
    pub edges: Vec<DebugGraphEdge>,
}

pub trait IDebugGraphService: Send + Sync {
    fn graph(&self) -> DebugGraph;
}
