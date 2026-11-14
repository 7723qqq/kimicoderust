use super::types::{Atom, AssociativeMemoryParams};

pub fn compare_atom_time(left: &Atom, right: &Atom) -> i64 {
    if left.created_at != right.created_at {
        return left.created_at - right.created_at;
    }
    if left.note_id != right.note_id {
        return 0;
    }
    left.span_start - right.span_start
}

pub fn stdp_drive(candidate: &Atom, other: &Atom, params: &AssociativeMemoryParams) -> f64 {
    let order = compare_atom_time(other, candidate);
    if order == 0 {
        return 0.0;
    }
    let distance = if other.note_id == candidate.note_id {
        (other.span_start - candidate.span_start).abs() as f64 / params.max_atom_chars as f64
    } else {
        (other.created_at - candidate.created_at).abs() as f64 / params.context_window_ms as f64
    };
    let decay = (-distance / params.stdp_tau).exp();
    (if order < 0 {
        params.stdp_a_plus
    } else {
        -params.stdp_a_minus
    }) * decay
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{AssociativeMemoryParams, NodeType};

    fn params() -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars: 100,
            context_window_ms: 1_000,
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
            vector_threshold: 0.5,
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
            supersede_threshold: 0.6,
            max_out_degree: 8,
            hetero_factor: 0.5,
            weight_dependence_k: 0.5,
            stdp_a_plus: 0.5,
            stdp_a_minus: 0.25,
            stdp_tau: 0.5,
            bcm_gain: 0.1,
            cascade_max_level: 2,
            cascade_strength: 0.5,
            sleep_downscale: 0.1,
            capture_count: 4,
            reconsolidate_merge_threshold: 0.9,
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

    fn atom(created_at: i64, note_id: &str, span_start: i64) -> Atom {
        Atom {
            id: format!("{note_id}-{span_start}"),
            note_id: note_id.into(),
            scope_key: "s".into(),
            node_type: NodeType::Atom,
            text: "text".into(),
            span_start,
            span_end: span_start + 4,
            terms: vec!["term".into()],
            vector: serde_json::Map::new(),
            dense_vector: None,
            concept_id: None,
            base_weight: 1.0,
            confidence: 0.5,
            replay_priority: 0,
            pinned: false,
            suppressed: false,
            valid_from: 0,
            valid_until: None,
            is_deleted: false,
            created_at,
            last_activated_at: 0,
        }
    }

    #[test]
    fn the_time_order_falls_back_to_note_and_span() {
        assert_eq!(compare_atom_time(&atom(1, "n", 0), &atom(2, "n", 0)), -1);
        assert_eq!(compare_atom_time(&atom(1, "a", 0), &atom(1, "b", 0)), 0);
        assert_eq!(compare_atom_time(&atom(1, "n", 4), &atom(1, "n", 9)), -5);
    }

    #[test]
    fn the_drive_sign_follows_the_causal_order() {
        let params = params();
        let earlier = atom(1, "n", 0);
        let later = atom(2, "n", 0);
        assert!(stdp_drive(&later, &earlier, &params) > 0.0);
        assert!(stdp_drive(&earlier, &later, &params) < 0.0);
        assert_eq!(stdp_drive(&earlier, &earlier, &params), 0.0);
        assert_eq!(stdp_drive(&atom(1, "a", 0), &atom(1, "b", 0), &params), 0.0);
    }

    #[test]
    fn same_note_distances_use_the_span_and_the_atom_cap() {
        let params = params();
        let drive = stdp_drive(&atom(0, "n", 100), &atom(0, "n", 0), &params);
        let expected = params.stdp_a_plus * (-(100.0 / 100.0) / params.stdp_tau).exp();
        assert!((drive - expected).abs() < 1e-12, "{drive} vs {expected}");
    }

    #[test]
    fn cross_note_distances_use_the_wall_clock_and_the_context_window() {
        let params = params();
        let drive = stdp_drive(&atom(1_000, "a", 0), &atom(0, "b", 0), &params);
        let expected = params.stdp_a_plus * (-(1_000.0 / 1_000.0) / params.stdp_tau).exp();
        assert!((drive - expected).abs() < 1e-12, "{drive} vs {expected}");
    }
}
