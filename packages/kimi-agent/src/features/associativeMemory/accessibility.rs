use super::decay::decay_factor;
use super::types::{Atom, AssociativeMemoryParams, Edge};

pub fn accessibility(
    atom: &Atom,
    edges: &[Edge],
    now: i64,
    params: &AssociativeMemoryParams,
) -> f64 {
    let mut incident = 0.0;
    for edge in edges {
        if edge.valid_until.is_some() {
            continue;
        }
        if edge.from != atom.id && edge.to != atom.id {
            continue;
        }
        incident += edge.weight;
    }
    let recency = decay_factor((now - atom.last_activated_at).max(0) as f64, params);
    recency * (1.0 + incident.ln_1p()) * atom.base_weight.max(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::NodeType;

    fn atom(id: &str, last_activated_at: i64, base_weight: f64) -> Atom {
        Atom {
            id: id.into(),
            note_id: "n1".into(),
            scope_key: "s".into(),
            node_type: NodeType::Atom,
            text: "text".into(),
            span_start: 0,
            span_end: 4,
            terms: vec!["text".into()],
            vector: serde_json::Map::new(),
            dense_vector: None,
            concept_id: None,
            base_weight,
            confidence: 0.5,
            replay_priority: 0,
            pinned: false,
            suppressed: false,
            valid_from: 0,
            valid_until: None,
            is_deleted: false,
            created_at: 1,
            last_activated_at,
        }
    }

    fn edge(from: &str, to: &str, weight: f64, valid_until: Option<i64>) -> Edge {
        Edge {
            id: format!("{from}-{to}"),
            from: from.into(),
            to: to.into(),
            kind: crate::features::associativeMemory::types::EdgeKind::Hebbian,
            weight,
            strengthen_count: 1,
            valid_from: 0,
            valid_until,
            created_at: 0,
            last_used_at: 0,
        }
    }

    fn params() -> AssociativeMemoryParams {
        crate::features::associativeMemory::params::associative_memory_params()
    }

    #[test]
    fn incident_weight_and_recency_drive_accessibility() {
        let params = params();
        let atom = atom("a", 0, 1.0);
        let edges = vec![
            edge("a", "b", 2.0, None),
            edge("b", "a", 3.0, None),
            edge("a", "c", 99.0, Some(10)),
            edge("x", "y", 100.0, None),
        ];
        let with_edges = accessibility(&atom, &edges, 0, &params);
        let without = accessibility(&atom, &[], 0, &params);
        assert!(with_edges > without);
        let expected = 1.0 * (1.0 + 5.0_f64.ln_1p()) * 1.0;
        assert!((with_edges - expected).abs() < 1e-12);
    }

    #[test]
    fn a_heavy_base_weight_and_stale_recency_both_show_up() {
        let params = params();
        let fresh = accessibility(&atom("a", 0, 2.0), &[], 0, &params);
        assert!((fresh - 2.0).abs() < 1e-12);
        let stale = accessibility(&atom("a", 0, 1.0), &[], params.half_life_ms * 4, &params);
        assert!(stale < 1.0);
    }
}
