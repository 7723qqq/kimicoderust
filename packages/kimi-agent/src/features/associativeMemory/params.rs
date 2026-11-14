use super::types::AssociativeMemoryParams;

pub fn associative_memory_params() -> AssociativeMemoryParams {
    AssociativeMemoryParams {
        max_atom_chars: 400,
        context_window_ms: 24 * 60 * 60 * 1000,
        base_learning_rate: 1.0,
        out_weight_budget: 50.0,
        max_edge_weight: 20.0,
        spread_hops: 3,
        spread_decay: 0.5,
        inhibition_factor: 0.5,
        base_k: 5.0,
        k_decay: 1.0,
        compute_budget: 500,
        seed_top_m: 50,
        episode_top_n: 3,
        list_limit: 50,
        top_k: 8,
        activation_threshold: 0.1,
        vector_threshold: 0.3,
        cluster_cooc_threshold: 3.0,
        cluster_min_size: 3,
        cluster_max_size: 20,
        procedure_min_steps: 3,
        procedure_threshold: 2.0,
        load_ref: 10.0,
        concept_formation_rate: 0.5,
        power_law_days: 30.0,
        half_life_ms: 30 * 24 * 60 * 60 * 1000,
        prune_threshold: 0.05,
        injection_max_tokens: 600,
        supersede_threshold: 0.9,
        max_out_degree: 12,
        hetero_factor: 0.1,
        weight_dependence_k: 1.0,
        stdp_a_plus: 0.5,
        stdp_a_minus: 1.2,
        stdp_tau: 1.0,
        bcm_gain: 0.3,
        cascade_max_level: 4,
        cascade_strength: 0.5,
        sleep_downscale: 0.9,
        capture_count: 3,
        reconsolidate_merge_threshold: 0.95,
        reconsolidate_novel_threshold: 0.2,
        interference_forgetting: 0.95,
        replay_strata: 3,
        motivated_forgetting: 0.95,
        motivated_forgetting_ms: 7 * 24 * 60 * 60 * 1000,
        schema_min_hops: 2,
        schema_top_k: 8,
        schema_ttl_ms: 7 * 24 * 60 * 60 * 1000,
        interference_max_edges: 24,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_default_matches_v2() {
        let params = associative_memory_params();
        assert_eq!(params.max_atom_chars, 400);
        assert_eq!(params.context_window_ms, 24 * 60 * 60 * 1000);
        assert_eq!(params.base_learning_rate, 1.0);
        assert_eq!(params.out_weight_budget, 50.0);
        assert_eq!(params.max_edge_weight, 20.0);
        assert_eq!(params.spread_hops, 3);
        assert_eq!(params.spread_decay, 0.5);
        assert_eq!(params.inhibition_factor, 0.5);
        assert_eq!(params.base_k, 5.0);
        assert_eq!(params.k_decay, 1.0);
        assert_eq!(params.compute_budget, 500);
        assert_eq!(params.seed_top_m, 50);
        assert_eq!(params.episode_top_n, 3);
        assert_eq!(params.list_limit, 50);
        assert_eq!(params.top_k, 8);
        assert_eq!(params.activation_threshold, 0.1);
        assert_eq!(params.vector_threshold, 0.3);
        assert_eq!(params.cluster_cooc_threshold, 3.0);
        assert_eq!(params.cluster_min_size, 3);
        assert_eq!(params.cluster_max_size, 20);
        assert_eq!(params.procedure_min_steps, 3);
        assert_eq!(params.procedure_threshold, 2.0);
        assert_eq!(params.load_ref, 10.0);
        assert_eq!(params.concept_formation_rate, 0.5);
        assert_eq!(params.power_law_days, 30.0);
        assert_eq!(params.half_life_ms, 30 * 24 * 60 * 60 * 1000);
        assert_eq!(params.prune_threshold, 0.05);
        assert_eq!(params.injection_max_tokens, 600);
        assert_eq!(params.supersede_threshold, 0.9);
        assert_eq!(params.max_out_degree, 12);
        assert_eq!(params.hetero_factor, 0.1);
        assert_eq!(params.weight_dependence_k, 1.0);
        assert_eq!(params.stdp_a_plus, 0.5);
        assert_eq!(params.stdp_a_minus, 1.2);
        assert_eq!(params.stdp_tau, 1.0);
        assert_eq!(params.bcm_gain, 0.3);
        assert_eq!(params.cascade_max_level, 4);
        assert_eq!(params.cascade_strength, 0.5);
        assert_eq!(params.sleep_downscale, 0.9);
        assert_eq!(params.capture_count, 3);
        assert_eq!(params.reconsolidate_merge_threshold, 0.95);
        assert_eq!(params.reconsolidate_novel_threshold, 0.2);
        assert_eq!(params.interference_forgetting, 0.95);
        assert_eq!(params.replay_strata, 3);
        assert_eq!(params.motivated_forgetting, 0.95);
        assert_eq!(params.motivated_forgetting_ms, 7 * 24 * 60 * 60 * 1000);
        assert_eq!(params.schema_min_hops, 2);
        assert_eq!(params.schema_top_k, 8);
        assert_eq!(params.schema_ttl_ms, 7 * 24 * 60 * 60 * 1000);
        assert_eq!(params.interference_max_edges, 24);
    }
}
