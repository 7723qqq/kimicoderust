use std::collections::{HashMap, HashSet};

use super::state::cascade_level;
use super::types::{AssociativeMemoryParams, Cooccurrence, Edge, EdgeKind};

pub const POWER_LAW_ALPHA: f64 = 0.5;
pub const CONSOLIDATION_STRENGTH: f64 = 0.5;
pub const SALIENCE_DECAY_STRENGTH: f64 = 0.2;

const MS_PER_DAY: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

pub fn decay_factor(age_ms: f64, params: &AssociativeMemoryParams) -> f64 {
    let t = params.power_law_days * MS_PER_DAY;
    if age_ms <= t {
        return (1.0 + age_ms / t).powf(-POWER_LAW_ALPHA);
    }
    let boundary = 2.0_f64.powf(-POWER_LAW_ALPHA);
    boundary * 0.5_f64.powf((age_ms - t) / params.half_life_ms as f64)
}

pub fn apply_decay(
    edges: &[Edge],
    now: i64,
    params: &AssociativeMemoryParams,
    pinned_ids: Option<&HashSet<String>>,
    decay_scale: f64,
    salience: Option<&HashMap<String, f64>>,
) -> Vec<Edge> {
    edges
        .iter()
        .map(|edge| {
            if pinned_ids.is_some_and(|ids| ids.contains(&edge.from) || ids.contains(&edge.to)) {
                return edge.clone();
            }
            let age = (now - edge.last_used_at) as f64;
            if age <= 0.0 {
                return edge.clone();
            }
            let endpoint_salience = (salience.and_then(|map| map.get(&edge.from)).copied().unwrap_or(1.0))
                .max(salience.and_then(|map| map.get(&edge.to)).copied().unwrap_or(1.0))
                .max(1.0);
            let consolidation = 1.0
                / (1.0
                    + cascade_level(edge.strengthen_count, params.cascade_max_level) as f64
                        * params.cascade_strength
                    + (endpoint_salience - 1.0) * SALIENCE_DECAY_STRENGTH);
            let effective_age = age * consolidation;
            Edge {
                weight: edge.weight * decay_factor(effective_age, params).powf(decay_scale),
                ..edge.clone()
            }
        })
        .collect()
}

pub fn apply_sleep_downscale(
    edges: &[Edge],
    params: &AssociativeMemoryParams,
) -> Vec<Edge> {
    edges
        .iter()
        .map(|edge| {
            if edge.kind == EdgeKind::Hebbian {
                Edge {
                    weight: edge.weight * params.sleep_downscale,
                    ..edge.clone()
                }
            } else {
                edge.clone()
            }
        })
        .collect()
}

pub fn restore_captured(edges: &[Edge], params: &AssociativeMemoryParams) -> Vec<Edge> {
    let inverse = 1.0 / params.sleep_downscale;
    edges
        .iter()
        .map(|edge| {
            if edge.kind == EdgeKind::Hebbian && edge.strengthen_count >= params.capture_count {
                Edge {
                    weight: params.max_edge_weight.min(edge.weight * inverse),
                    ..edge.clone()
                }
            } else {
                edge.clone()
            }
        })
        .collect()
}

pub fn apply_motivated_forgetting(
    edges: &[Edge],
    neglected: &HashSet<String>,
    params: &AssociativeMemoryParams,
) -> Vec<Edge> {
    edges
        .iter()
        .map(|edge| {
            if edge.kind == EdgeKind::Hebbian && neglected.contains(&edge.from) {
                Edge {
                    weight: edge.weight * params.motivated_forgetting,
                    ..edge.clone()
                }
            } else {
                edge.clone()
            }
        })
        .collect()
}

pub fn prune_edges(edges: &[Edge], params: &AssociativeMemoryParams) -> Vec<Edge> {
    edges
        .iter()
        .filter(|edge| edge.weight >= params.prune_threshold)
        .cloned()
        .collect()
}

pub fn prune_cooccurrences(
    coocs: &[Cooccurrence],
    now: i64,
    params: &AssociativeMemoryParams,
    threshold: f64,
) -> Vec<Cooccurrence> {
    coocs
        .iter()
        .filter(|cooc| {
            (cooc.count as f64) >= threshold || now - cooc.last_at <= params.half_life_ms
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::AssociativeMemoryParams;

    fn params() -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars: 400,
            context_window_ms: 86_400_000,
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
            half_life_ms: 30 * 86_400_000,
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
            motivated_forgetting_ms: 7 * 86_400_000,
            schema_min_hops: 2,
            schema_top_k: 8,
            schema_ttl_ms: 7 * 86_400_000,
            interference_max_edges: 24,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind, weight: f64, count: i64, last_used_at: i64) -> Edge {
        Edge {
            id: format!("{from}-{to}"),
            from: from.into(),
            to: to.into(),
            kind,
            weight,
            strengthen_count: count,
            valid_from: 0,
            valid_until: None,
            created_at: 0,
            last_used_at,
        }
    }

    #[test]
    fn power_law_decay_follows_v2_two_regimes() {
        let params = params();
        let t = params.power_law_days * MS_PER_DAY;
        assert!((decay_factor(0.0, &params) - 1.0).abs() < 1e-12);
        let at_boundary = decay_factor(t, &params);
        assert!((at_boundary - 2.0_f64.powf(-POWER_LAW_ALPHA)).abs() < 1e-12);
        let beyond = decay_factor(t * 2.0, &params);
        assert!(beyond < at_boundary);
        assert!(decay_factor(1e12, &params) >= 0.0);
    }

    #[test]
    fn pinned_endpoints_skip_decay() {
        let params = params();
        let pinned = HashSet::from(["a".to_string()]);
        let edges = vec![
            edge("a", "b", EdgeKind::Hebbian, 1.0, 1, 0),
            edge("b", "c", EdgeKind::Hebbian, 1.0, 1, 0),
        ];
        let out = apply_decay(&edges, 10_000, &params, Some(&pinned), 1.0, None);
        assert_eq!(out[0].weight, 1.0);
        assert!(out[1].weight < 1.0);
    }

    #[test]
    fn non_positive_age_keeps_the_weight() {
        let params = params();
        let edges = vec![edge("a", "b", EdgeKind::Hebbian, 1.0, 1, 50)];
        let out = apply_decay(&edges, 50, &params, None, 1.0, None);
        assert_eq!(out[0].weight, 1.0);
    }

    #[test]
    fn salience_slows_the_effective_age() {
        let params = params();
        let mut salience = HashMap::new();
        salience.insert("a".to_string(), 3.0);
        let edges = vec![edge("a", "b", EdgeKind::Hebbian, 1.0, 1, 0)];
        let plain = apply_decay(&edges, 100_000, &params, None, 1.0, None);
        let damped = apply_decay(&edges, 100_000, &params, None, 1.0, Some(&salience));
        assert!(damped[0].weight > plain[0].weight);
    }

    #[test]
    fn sleep_downscale_touches_only_hebbian_edges() {
        let params = params();
        let edges = vec![
            edge("a", "b", EdgeKind::Hebbian, 1.0, 1, 0),
            edge("a", "b", EdgeKind::Concept, 1.0, 1, 0),
        ];
        let out = apply_sleep_downscale(&edges, &params);
        assert_eq!(out[0].weight, params.sleep_downscale);
        assert_eq!(out[1].weight, 1.0);
    }

    #[test]
    fn captured_edges_restore_up_to_the_weight_cap() {
        let params = params();
        let edges = vec![
            edge("a", "b", EdgeKind::Hebbian, 1.0, 2, 0),
            edge("c", "d", EdgeKind::Hebbian, 1.0, 3, 0),
            edge("e", "f", EdgeKind::Supersede, 1.0, 9, 0),
        ];
        let out = restore_captured(&edges, &params);
        assert_eq!(out[0].weight, 1.0, "below captureCount stays put");
        assert!(
            (out[1].weight - (1.0 / params.sleep_downscale)).abs() < 1e-12,
            "the restored weight is capped by maxEdgeWeight, not raised to it"
        );
        assert_eq!(out[2].weight, 1.0);
    }

    #[test]
    fn motivated_forgetting_only_hits_neglected_sources() {
        let params = params();
        let neglected = HashSet::from(["a".to_string()]);
        let edges = vec![
            edge("a", "b", EdgeKind::Hebbian, 1.0, 1, 0),
            edge("b", "a", EdgeKind::Hebbian, 1.0, 1, 0),
        ];
        let out = apply_motivated_forgetting(&edges, &neglected, &params);
        assert_eq!(out[0].weight, params.motivated_forgetting);
        assert_eq!(out[1].weight, 1.0);
    }

    #[test]
    fn pruning_uses_the_v2_thresholds() {
        let params = params();
        let edges = vec![
            edge("a", "b", EdgeKind::Hebbian, 0.05, 1, 0),
            edge("c", "d", EdgeKind::Hebbian, 0.04, 1, 0),
        ];
        let kept = prune_edges(&edges, &params);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].from, "a");

        let coocs = vec![
            Cooccurrence {
                key: "k1".into(),
                a: "a".into(),
                b: "b".into(),
                count: 2,
                last_at: 0,
            },
            Cooccurrence {
                key: "k2".into(),
                a: "c".into(),
                b: "d".into(),
                count: 1,
                last_at: 0,
            },
        ];
        let kept = prune_cooccurrences(&coocs, 0, &params, 2.0);
        assert_eq!(
            kept.len(),
            2,
            "a fresh cooc survives even below the count threshold"
        );
        let kept = prune_cooccurrences(&coocs, params.half_life_ms + 1, &params, 2.0);
        assert_eq!(kept.len(), 1, "the stale low-count cooc is dropped");
        assert_eq!(kept[0].key, "k1");
    }
}
