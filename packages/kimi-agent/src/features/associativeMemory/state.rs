use std::collections::HashMap;

use super::types::{AssociativeMemoryParams, Edge};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NodeState {
    pub activity: f64,
    pub theta_m: f64,
    pub cascade_level: usize,
}

pub fn cascade_level(strengthen_count: i64, max_level: i64) -> usize {
    let bounded = strengthen_count.max(0) as f64;
    let level = (1.0 + bounded).log2().floor();
    (level.max(0.0) as i64).min(max_level) as usize
}

pub fn derive_node_states(
    edges: &[Edge],
    activity: &HashMap<String, f64>,
    params: &AssociativeMemoryParams,
) -> HashMap<String, NodeState> {
    let mut totals: HashMap<&str, (f64, usize)> = HashMap::new();
    for edge in edges {
        if edge.kind != super::types::EdgeKind::Hebbian {
            continue;
        }
        let entry = totals.entry(edge.from.as_str()).or_insert((0.0, 0));
        entry.0 += edge.strengthen_count as f64;
        entry.1 += 1;
    }
    let mut states = HashMap::new();
    let mut sources: std::collections::HashSet<&str> = totals.keys().copied().collect();
    sources.extend(activity.keys().map(String::as_str));
    for source in sources {
        let (sum, count) = totals.get(source).copied().unwrap_or((0.0, 0));
        let average = if count == 0 { 0.0 } else { sum / count as f64 };
        states.insert(
            source.to_string(),
            NodeState {
                activity: activity.get(source).copied().unwrap_or(0.0),
                theta_m: average,
                cascade_level: cascade_level(
                    average.round() as i64,
                    params.cascade_max_level,
                ),
            },
        );
    }
    states
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{EdgeKind, NodeType};
    use crate::features::associativeMemory::types::AssociativeMemoryParams;

    fn params() -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars: 4096,
            context_window_ms: 60_000,
            base_learning_rate: 0.05,
            out_weight_budget: 4.0,
            max_edge_weight: 1.0,
            spread_hops: 2,
            spread_decay: 0.5,
            inhibition_factor: 0.3,
            base_k: 8.0,
            k_decay: 0.5,
            compute_budget: 200,
            seed_top_m: 5,
            episode_top_n: 8,
            list_limit: 20,
            top_k: 5,
            activation_threshold: 0.2,
            vector_threshold: 0.3,
            cluster_cooc_threshold: 0.5,
            cluster_min_size: 2,
            cluster_max_size: 8,
            procedure_min_steps: 2,
            procedure_threshold: 0.4,
            load_ref: 8.0,
            concept_formation_rate: 0.5,
            power_law_days: 7.0,
            half_life_ms: 86_400_000,
            prune_threshold: 0.05,
            injection_max_tokens: 800,
            supersede_threshold: 0.8,
            max_out_degree: 8,
            hetero_factor: 0.5,
            weight_dependence_k: 0.5,
            stdp_a_plus: 0.1,
            stdp_a_minus: 0.1,
            stdp_tau: 20.0,
            bcm_gain: 0.1,
            cascade_max_level: 2,
            cascade_strength: 0.5,
            sleep_downscale: 0.1,
            capture_count: 4,
            reconsolidate_merge_threshold: 0.8,
            reconsolidate_novel_threshold: 0.2,
            interference_forgetting: 0.1,
            replay_strata: 3,
            motivated_forgetting: 0.1,
            motivated_forgetting_ms: 86_400_000,
            schema_min_hops: 2,
            schema_top_k: 5,
            schema_ttl_ms: 3_600_000,
            interference_max_edges: 8,
        }
    }

    fn edge(id: &str, from: &str, to: &str, kind: EdgeKind, weight: f64, count: i64) -> Edge {
        Edge {
            id: id.into(),
            from: from.into(),
            to: to.into(),
            kind,
            weight,
            strengthen_count: count,
            valid_from: 0,
            valid_until: None,
            created_at: 0,
            last_used_at: 0,
        }
    }

    #[test]
    fn cascade_levels_grow_logarithmically_and_clamp() {
        assert_eq!(cascade_level(0, 4), 0);
        assert_eq!(cascade_level(1, 4), 1);
        assert_eq!(cascade_level(3, 4), 2);
        assert_eq!(cascade_level(7, 4), 3);
        assert_eq!(cascade_level(10_000, 4), 4, "clamped at max_level");
        assert_eq!(cascade_level(-5, 4), 0);
    }

    #[test]
    fn theta_is_the_mean_strengthen_count_of_hebbian_edges() {
        let edges = vec![
            edge("e1", "a", "b", EdgeKind::Hebbian, 0.5, 2),
            edge("e2", "a", "c", EdgeKind::Hebbian, 0.5, 4),
            edge("e3", "a", "d", EdgeKind::Inhibits, 1.0, 9),
        ];
        let mut activity = HashMap::new();
        activity.insert("a".to_string(), 1.5);
        activity.insert("z".to_string(), 0.5);
        let states = derive_node_states(&edges, &activity, &params());
        assert_eq!(states["a"].theta_m, 3.0);
        assert_eq!(states["a"].activity, 1.5);
        assert_eq!(states["a"].cascade_level, 2, "floor(log2(1+3))=2");
        assert_eq!(states["z"].theta_m, 0.0);
        assert_eq!(states["z"].activity, 0.5);
    }

    #[test]
    fn an_edge_without_a_hebbian_history_has_no_state() {
        assert!(derive_node_states(&[], &HashMap::new(), &params()).is_empty());
        let _ = NodeType::Atom;
    }
}
