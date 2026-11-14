use std::collections::HashMap;

use super::terms::{is_stop_term, related_terms};
use super::types::{Atom, AssociativeMemoryParams};
use super::vector::cosine;

pub fn shares_content_term(left: &Atom, right: &Atom) -> bool {
    if left.terms.is_empty() || right.terms.is_empty() {
        return false;
    }
    for term in &left.terms {
        if is_stop_term(term) {
            continue;
        }
        for other in &right.terms {
            if is_stop_term(other) {
                continue;
            }
            if related_terms(term, other) {
                return true;
            }
        }
    }
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemorySeparationDecision {
    Merge,
    Separate,
    New,
}

pub fn memory_separation_decision(
    candidate: &Atom,
    existing: &Atom,
    params: &AssociativeMemoryParams,
) -> MemorySeparationDecision {
    let to_map = |atom: &Atom| {
        atom.vector
            .iter()
            .filter_map(|(term, value)| value.as_f64().map(|number| (term.clone(), number)))
            .collect::<HashMap<String, f64>>()
    };
    let similarity = cosine(&to_map(candidate), &to_map(existing));
    if similarity >= params.reconsolidate_merge_threshold {
        return MemorySeparationDecision::Merge;
    }
    if similarity >= params.supersede_threshold && shares_content_term(candidate, existing) {
        return MemorySeparationDecision::Separate;
    }
    if similarity >= params.vector_threshold && shares_content_term(candidate, existing) {
        return MemorySeparationDecision::Merge;
    }
    MemorySeparationDecision::New
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{NodeType};

    fn atom(id: &str, terms: &[&str], weight: f64) -> Atom {
        let mut vector = serde_json::Map::new();
        for term in terms {
            vector.insert((*term).to_string(), serde_json::json!(weight));
        }
        Atom {
            id: id.into(),
            note_id: "n1".into(),
            scope_key: "s".into(),
            node_type: NodeType::Atom,
            text: terms.join(" "),
            span_start: 0,
            span_end: terms.join(" ").len() as i64,
            terms: terms.iter().map(|term| term.to_string()).collect(),
            vector,
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
            created_at: 1,
            last_activated_at: 0,
        }
    }

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
            stdp_a_plus: 0.1,
            stdp_a_minus: 0.1,
            stdp_tau: 20.0,
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

    #[test]
    fn a_shared_non_stop_term_connects_two_atoms() {
        assert!(shares_content_term(
            &atom("a", &["snapshot", "graph"], 1.0),
            &atom("b", &["snap"], 1.0)
        ));
        assert!(!shares_content_term(
            &atom("a", &["the", "and"], 1.0),
            &atom("b", &["of", "to"], 1.0)
        ));
        assert!(!shares_content_term(&atom("a", &[], 1.0), &atom("b", &["x"], 1.0)));
        assert!(!shares_content_term(&atom("a", &["x"], 1.0), &atom("b", &[], 1.0)));
    }

    #[test]
    fn separation_decisions_follow_the_v2_threshold_order() {
        let params = params();
        let candidate = atom("a", &["snap"], 1.0);
        assert_eq!(
            memory_separation_decision(&candidate, &atom("b", &["snap"], 1.0), &params),
            MemorySeparationDecision::Merge,
            "a very similar atom merges"
        );
        let distant = atom("b", &["rust", "graph"], 0.9);
        assert_eq!(
            memory_separation_decision(&candidate, &distant, &params),
            MemorySeparationDecision::New,
            "below the vector threshold nothing connects"
        );
        let overlapping = atom("c", &["snap", "edge"], 0.9);
        assert_eq!(
            memory_separation_decision(&candidate, &overlapping, &params),
            MemorySeparationDecision::Separate,
            "between supersede and merge with a shared term"
        );
    }
}
