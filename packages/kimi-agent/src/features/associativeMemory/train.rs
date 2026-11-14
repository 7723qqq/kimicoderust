use indexmap::{IndexMap, IndexSet};
use std::collections::{HashMap, HashSet};

use super::separation::shares_content_term;
use super::state::derive_node_states;
use super::terms::compare_by_text;
use super::temporal::stdp_drive;
use super::types::{
    AssociativeMemoryParams, Edge, EdgeKind,
};

#[derive(Debug, Clone, PartialEq)]
pub struct SupersededEntry {
    pub valid_until: i64,
    pub suppressed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrainResult {
    pub edges: Vec<Edge>,
    pub deleted: Vec<String>,
    pub superseded: IndexMap<String, SupersededEntry>,
}

struct Index {
    ids: IndexMap<String, Edge>,
    by_source: HashMap<String, IndexSet<String>>,
}

impl Index {
    fn new(edges: &[Edge]) -> Self {
        let mut index = Self {
            ids: IndexMap::new(),
            by_source: HashMap::new(),
        };
        for edge in edges {
            index.insert(edge);
        }
        index
    }

    fn insert(&mut self, edge: &Edge) {
        self.ids.insert(edge.id.clone(), edge.clone());
        self.by_source
            .entry(edge.from.clone())
            .or_default()
            .insert(edge.id.clone());
    }

    fn remove(&mut self, id: &str) {
        self.ids.shift_remove(id);
    }
}

#[allow(clippy::too_many_arguments)]
fn hebbian_edge(
    id: String,
    from: &str,
    to: &str,
    weight: f64,
    strengthen_count: i64,
    valid_from: i64,
    created_at: i64,
    now: i64,
) -> Edge {
    Edge {
        id,
        from: from.to_string(),
        to: to.to_string(),
        kind: EdgeKind::Hebbian,
        weight,
        strengthen_count,
        valid_from,
        valid_until: None,
        created_at,
        last_used_at: now,
    }
}

fn fixed_edge(id: String, from: &str, to: &str, kind: EdgeKind, now: i64) -> Edge {
    Edge {
        id,
        from: from.to_string(),
        to: to.to_string(),
        kind,
        weight: 1.0,
        strengthen_count: 1,
        valid_from: now,
        valid_until: None,
        created_at: now,
        last_used_at: now,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn hebbian_connect<F>(
    new_atoms: &[super::types::Atom],
    context_atoms: &[super::types::Atom],
    edges: &[Edge],
    params: &AssociativeMemoryParams,
    activity: &HashMap<String, f64>,
    now: i64,
    contradiction: F,
) -> TrainResult
where
    F: Fn(&super::types::Atom, &super::types::Atom) -> bool,
{
    let mut index = Index::new(edges);
    let stored: HashSet<String> = edges.iter().map(|edge| edge.id.clone()).collect();
    let mut labels: HashMap<String, String> = HashMap::new();
    for atom in context_atoms {
        labels.insert(atom.id.clone(), atom.text.clone());
    }
    for atom in new_atoms {
        labels.insert(atom.id.clone(), atom.text.clone());
    }
    let mut superseded: IndexMap<String, SupersededEntry> = IndexMap::new();
    let mut deleted: IndexSet<String> = IndexSet::new();
    let node_states = derive_node_states(edges, activity, params);

    for atom in new_atoms {
        let source = atom.id.as_str();
        let mut order: Vec<(String, f64, f64)> = Vec::new();
        let node_factor = 1.0 / (1.0 + activity.get(source).copied().unwrap_or(0.0));

        for other in context_atoms {
            if source == other.id {
                continue;
            }
            if !shares_content_term(atom, other) {
                continue;
            }
            let id = format!("{source}>{}", other.id);
            if contradiction(atom, other) {
                superseded.insert(
                    other.id.clone(),
                    SupersededEntry {
                        valid_until: now,
                        suppressed: true,
                    },
                );
                if !index.ids.contains_key(&id) {
                    let edge = fixed_edge(id.clone(), source, &other.id, EdgeKind::Supersede, now);
                    index.insert(&edge);
                    order.push((id.clone(), 0.0, 1.0));
                }
                let inhibit_id = format!("{source}~{}", other.id);
                if !index.ids.contains_key(&inhibit_id) {
                    let edge = fixed_edge(
                        inhibit_id.clone(),
                        source,
                        &other.id,
                        EdgeKind::Inhibits,
                        now,
                    );
                    index.insert(&edge);
                    order.push((inhibit_id, 0.0, 1.0));
                }
                continue;
            }
            let existing = index.ids.get(&id).cloned();
            let strengthen_count = existing.as_ref().map(|edge| edge.strengthen_count).unwrap_or(0);
            let room = (1.0 - existing.as_ref().map(|edge| edge.weight).unwrap_or(0.0) / params.max_edge_weight).max(0.0);
            let drive = stdp_drive(atom, other, params);
            let state = node_states.get(source);
            let bcm = if strengthen_count == 0
                || state.is_none()
                || state.is_some_and(|state| state.theta_m == 0.0)
            {
                0.0
            } else {
                let theta = state.map(|state| state.theta_m).unwrap_or(0.0);
                (params.bcm_gain * (strengthen_count as f64 - theta)) / (theta + 1.0)
            };
            let delta = (params.base_learning_rate / (1.0 + strengthen_count as f64))
                * node_factor
                * room
                * (1.0 + drive + bcm);
            let weight = ((existing.as_ref().map(|edge| edge.weight).unwrap_or(0.0)) + delta)
                .clamp(0.0, params.max_edge_weight);
            let edge = hebbian_edge(
                id.clone(),
                source,
                &other.id,
                weight,
                if delta > 0.0 { strengthen_count + 1 } else { strengthen_count },
                existing.as_ref().map(|edge| edge.valid_from).unwrap_or(now),
                existing.as_ref().map(|edge| edge.created_at).unwrap_or(now),
                now,
            );
            index.insert(&edge);
            order.push((id, delta.max(0.0), weight));
        }

        let mut step: HashMap<String, usize> = HashMap::new();
        for (position, entry) in order.iter().enumerate() {
            step.insert(entry.0.clone(), position);
        }
        let total_delta: f64 = order.iter().map(|entry| entry.1).sum();
        let depression = |weight: f64, competition: f64| {
            1.0 / (1.0 + params.hetero_factor * competition * (weight / (weight + params.weight_dependence_k)))
        };
        if let Some(sources) = index.by_source.get(source).cloned() {
            for id in sources {
                let Some(edge) = index.ids.get(&id).cloned() else {
                    continue;
                };
                if edge.kind != EdgeKind::Hebbian {
                    continue;
                }
                let own_delta = step.get(&id).map(|position| order[*position].1).unwrap_or(0.0);
                let competition = (total_delta - own_delta).max(0.0);
                let weight = edge.weight * depression(edge.weight, competition);
                index.insert(&Edge { weight, ..edge });
            }
        }

        let mut out: Vec<Edge> = Vec::new();
        if let Some(sources) = index.by_source.get(source).cloned() {
            for id in sources {
                let Some(edge) = index.ids.get(&id).cloned() else {
                    continue;
                };
                if edge.valid_until.is_some() || edge.kind != EdgeKind::Hebbian {
                    continue;
                }
                out.push(edge);
            }
        }
        let total: f64 = out.iter().map(|edge| edge.weight).sum();
        if total > params.out_weight_budget {
            let scale = params.out_weight_budget / total;
            for edge in &out {
                index.insert(&Edge {
                    weight: edge.weight * scale,
                    ..edge.clone()
                });
            }
        }
        if out.len() > params.max_out_degree as usize {
            let mut ranked = out.clone();
            ranked.sort_by(|left, right| {
                right
                    .weight
                    .partial_cmp(&left.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        compare_by_text(
                            (left.to.as_str(), labels.get(&left.to).map(String::as_str).unwrap_or("")),
                            (right.to.as_str(), labels.get(&right.to).map(String::as_str).unwrap_or("")),
                        )
                    })
            });
            for edge in ranked.iter().skip(params.max_out_degree as usize) {
                index.remove(&edge.id);
                if let Some(sources) = index.by_source.get_mut(source) {
                    sources.shift_remove(&edge.id);
                }
                if stored.contains(&edge.id) {
                    deleted.insert(edge.id.clone());
                }
            }
        }
    }

    TrainResult {
        edges: index.ids.values().cloned().collect(),
        deleted: deleted.into_iter().collect(),
        superseded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::separation::MemorySeparationDecision;
    use crate::features::associativeMemory::separation::memory_separation_decision;
    use crate::features::associativeMemory::types::{Atom, AssociativeMemoryParams, NodeType};

    fn separate_contradiction<'a>(
        params: &'a AssociativeMemoryParams,
    ) -> impl Fn(&Atom, &Atom) -> bool + 'a {
        move |candidate, existing| {
            memory_separation_decision(candidate, existing, params)
                == MemorySeparationDecision::Separate
        }
    }

    fn params() -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars: 4096,
            context_window_ms: 1_000,
            base_learning_rate: 0.05,
            out_weight_budget: 4.0,
            max_edge_weight: 1.0,
            spread_hops: 2,
            spread_decay: 0.5,
            inhibition_factor: 0.0,
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

    fn atom(id: &str, text: &str, terms: &[&str]) -> Atom {
        let mut vector = serde_json::Map::new();
        for term in terms {
            vector.insert((*term).to_string(), serde_json::json!(1.0));
        }
        Atom {
            id: id.into(),
            note_id: "n1".into(),
            scope_key: "s".into(),
            node_type: NodeType::Atom,
            text: text.into(),
            span_start: 0,
            span_end: text.len() as i64,
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
            created_at: 0,
            last_activated_at: 0,
        }
    }

    #[test]
    fn a_shared_term_creates_a_hebbian_edge() {
        let params = params();
        let new_atoms = vec![atom("a", "snapshot graph", &["snapshot", "graph"])];
        let context = vec![atom("b", "snapshot only", &["snapshot", "edge"])];
        let result = hebbian_connect(
            &new_atoms,
            &context,
            &[],
            &params,
            &HashMap::new(),
            10,
            separate_contradiction(&params),
        );
        assert_eq!(result.edges.len(), 1);
        assert_eq!(result.edges[0].kind, EdgeKind::Hebbian);
        assert_eq!(result.edges[0].id, "a>b");
        assert!(result.edges[0].weight > 0.0);
        assert_eq!(result.edges[0].strengthen_count, 1);
    }

    #[test]
    fn a_separate_decision_writes_supersede_and_inhibits_edges() {
        let cfg = params();
        let mut separate = params();
        separate.reconsolidate_merge_threshold = 0.9;
        separate.supersede_threshold = 0.5;
        let new_atoms = vec![atom("a", "snapshot", &["snapshot"])];
        let context = vec![atom("b", "snapshot edge", &["snapshot", "edge"])];
        let result = hebbian_connect(
            &new_atoms,
            &context,
            &[],
            &separate,
            &HashMap::new(),
            10,
            separate_contradiction(&separate),
        );
        assert!(result.superseded.contains_key("b"));
        assert_eq!(
            result.edges.iter().map(|edge| edge.kind).collect::<Vec<_>>(),
            vec![EdgeKind::Supersede, EdgeKind::Inhibits]
        );
        assert_eq!(result.edges[0].weight, 1.0);
        let _ = cfg;
        let _ = MemorySeparationDecision::Separate;
    }

    #[test]
    fn unrelated_terms_produce_no_edges() {
        let result = hebbian_connect(
            &[atom("a", "rust", &["rust"])],
            &[atom("b", "graph", &["graph"])],
            &[],
            &params(),
            &HashMap::new(),
            10,
            separate_contradiction(&params()),
        );
        assert!(result.edges.is_empty());
    }

    #[test]
    fn the_out_weight_budget_scales_the_source_edges_down() {
        let mut params = params();
        params.out_weight_budget = 0.001;
        params.max_out_degree = 8;
        let new_atoms = vec![atom("a", "shared", &["shared"])];
        let context = vec![
            atom("b", "shared", &["shared"]),
            atom("c", "shared", &["shared"]),
        ];
        let result = hebbian_connect(
            &new_atoms,
            &context,
            &[],
            &params,
            &HashMap::new(),
            10,
            separate_contradiction(&params),
        );
        let total: f64 = result.edges.iter().map(|edge| edge.weight).sum();
        assert!(total <= params.out_weight_budget + 1e-9, "{total}");
    }

    #[test]
    fn an_over_full_source_keeps_the_strongest_edges() {
        let mut params = params();
        params.out_weight_budget = 100.0;
        params.max_out_degree = 2;
        params.base_learning_rate = 0.9;
        let new_atoms = vec![atom("a", "shared", &["shared"])];
        let context = vec![
            atom("b", "shared", &["shared"]),
            atom("c", "shared", &["shared"]),
            atom("d", "shared", &["shared"]),
        ];
        let result = hebbian_connect(
            &new_atoms,
            &context,
            &[],
            &params,
            &HashMap::new(),
            10,
            separate_contradiction(&params),
        );
        assert_eq!(result.edges.len(), 2);
    }
}
