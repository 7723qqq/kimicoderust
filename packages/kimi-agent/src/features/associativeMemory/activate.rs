use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::procedure::procedure_model_free_strength;
use super::terms::{is_stop_term, matches_any};
use super::types::{
    Atom, AssociativeMemoryParams, Concept, Edge, EdgeKind, NodeType, Procedure, RecallItem,
    RecallQuery, RecallResult, RecallTraceNode, TraceNodeKind, TraceEdge,
};
use super::vector::{average_vector, cosine, cosine_dense, term_frequency};

const CROSS_SCORE_TOP_N: usize = 12;
const SALIENCE_GAIN: f64 = 0.25;
const SALIENCE_CAP: f64 = 2.0;
const CROSS_SCORE_WEIGHT: f64 = 0.2;

#[derive(Debug, Clone)]
struct Neighbor {
    to: String,
    weight: f64,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    kind: NodeType,
    text: String,
    note_id: String,
    vector: serde_json::Map<String, Value>,
    dense_vector: Option<Vec<f64>>,
    terms: Vec<String>,
    confidence: f64,
    valid_from: i64,
    valid_until: Option<i64>,
    concept_id: Option<String>,
    suppressed: bool,
    is_deleted: bool,
    salience: f64,
    member_ids: Vec<String>,
}

fn to_map(vector: &serde_json::Map<String, Value>) -> HashMap<String, f64> {
    vector
        .iter()
        .filter_map(|(term, value)| value.as_f64().map(|number| (term.clone(), number)))
        .collect()
}

fn compare_label(labels: &HashMap<String, String>, left: &str, right: &str) -> std::cmp::Ordering {
    let left_label = labels.get(left).map(String::as_str).unwrap_or(left);
    let right_label = labels.get(right).map(String::as_str).unwrap_or(right);
    left_label
        .encode_utf16()
        .cmp(right_label.encode_utf16())
        .then_with(|| left.encode_utf16().cmp(right.encode_utf16()))
}

#[allow(clippy::too_many_arguments)]
pub fn activate(
    query: &RecallQuery,
    atoms: &[Atom],
    concepts: &[Concept],
    edges: &[Edge],
    params: &AssociativeMemoryParams,
    now: i64,
    procedures: &[Procedure],
    cue_dense: Option<&[f64]>,
) -> RecallResult {
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut atom_by_id: HashMap<&str, &Atom> = HashMap::new();
    for atom in atoms {
        atom_by_id.insert(atom.id.as_str(), atom);
        nodes.insert(
            atom.id.clone(),
            Node {
                id: atom.id.clone(),
                kind: NodeType::Atom,
                text: atom.text.clone(),
                note_id: atom.note_id.clone(),
                vector: atom.vector.clone(),
                dense_vector: atom.dense_vector.clone(),
                terms: atom.terms.clone(),
                confidence: atom.confidence,
                valid_from: atom.valid_from,
                valid_until: atom.valid_until,
                concept_id: atom.concept_id.clone(),
                suppressed: atom.suppressed,
                is_deleted: atom.is_deleted,
                salience: atom.base_weight,
                member_ids: Vec::new(),
            },
        );
    }
    for concept in concepts {
        let note_id = concept
            .atom_ids
            .first()
            .and_then(|id| atom_by_id.get(id.as_str()).copied())
            .map(|atom| atom.note_id.clone())
            .unwrap_or_default();
        let vector = match &concept.vector {
            Some(vector) => vector.clone(),
            None => {
                let member_vectors = concept
                    .atom_ids
                    .iter()
                    .filter_map(|id| atom_by_id.get(id.as_str()).copied())
                    .map(|atom| to_map(&atom.vector))
                    .collect::<Vec<_>>();
                average_vector(&member_vectors)
                    .into_iter()
                    .map(|(term, value)| (term, Value::from(value)))
                    .collect()
            }
        };
        nodes.insert(
            concept.id.clone(),
            Node {
                id: concept.id.clone(),
                kind: NodeType::Concept,
                text: concept.label.clone(),
                note_id,
                vector,
                dense_vector: concept.dense_vector.clone(),
                terms: concept.terms.clone(),
                confidence: 1.0,
                valid_from: concept.valid_from,
                valid_until: concept.valid_until,
                concept_id: None,
                suppressed: concept.suppressed,
                is_deleted: false,
                salience: 1.0,
                member_ids: concept.atom_ids.clone(),
            },
        );
    }
    for procedure in procedures {
        if procedure.suppressed {
            continue;
        }
        nodes.insert(
            procedure.id.clone(),
            Node {
                id: procedure.id.clone(),
                kind: NodeType::Procedure,
                text: procedure.label.clone(),
                note_id: procedure.source_note_id.clone(),
                vector: procedure.vector.clone(),
                dense_vector: None,
                terms: procedure.terms.clone(),
                confidence: 1.0,
                valid_from: procedure.created_at,
                valid_until: None,
                concept_id: None,
                suppressed: false,
                is_deleted: false,
                salience: procedure_model_free_strength(procedure),
                member_ids: Vec::new(),
            },
        );
    }

    let mut labels: HashMap<String, String> = HashMap::new();
    for node in nodes.values() {
        labels.insert(node.id.clone(), node.text.clone());
    }
    let mut note_labels: HashMap<String, String> = HashMap::new();
    for atom in atoms {
        note_labels
            .entry(atom.note_id.clone())
            .and_modify(|current| {
                if atom.text.encode_utf16().cmp(current.encode_utf16()).is_lt() {
                    *current = atom.text.clone();
                }
            })
            .or_insert_with(|| atom.text.clone());
    }

    let is_active = |node: &Node| -> bool {
        if node.is_deleted {
            return false;
        }
        if query.include_history {
            return true;
        }
        if node.suppressed {
            return false;
        }
        match node.valid_until {
            None => true,
            Some(valid_until) => valid_until > now,
        }
    };

    let cue_terms: Vec<String> = crate::tools::memory_search::tokenize(&query.cue)
        .into_iter()
        .filter(|term| term.len() <= 0xffff && !is_stop_term(term))
        .collect();
    let cue_set: HashSet<&str> = cue_terms.iter().map(String::as_str).collect();
    let cue_vector = term_frequency(&cue_terms);

    let concept_coverage = |node: &Node| -> f64 {
        if node.kind != NodeType::Concept || node.member_ids.is_empty() {
            return 1.0;
        }
        let mut matched = 0usize;
        for member_id in &node.member_ids {
            let Some(member) = nodes.get(member_id) else {
                continue;
            };
            if !member
                .terms
                .iter()
                .any(|term| matches_any(term, &cue_terms))
            {
                continue;
            }
            matched += 1;
            if matched >= 2 {
                break;
            }
        }
        ((matched as f64) / 2.0).min(1.0)
    };

    let mut term_seeds: HashMap<String, f64> = HashMap::new();
    let mut vector_seeds: HashMap<String, f64> = HashMap::new();
    for node in nodes.values() {
        if !is_active(node) {
            continue;
        }
        let mut matched = 0usize;
        let mut weight = 0.0;
        for term in &node.terms {
            if !matches_any(term, &cue_terms) {
                continue;
            }
            matched += 1;
            weight += node.vector.get(term).and_then(Value::as_f64).unwrap_or(0.0);
        }
        let overlap = if cue_set.is_empty() {
            0.0
        } else {
            matched as f64 / cue_set.len() as f64
        };
        let vector_score = match (node.dense_vector.as_deref(), cue_dense) {
            (Some(dense), Some(cue)) => cosine_dense(dense, cue),
            _ => cosine(&cue_vector, &to_map(&node.vector)),
        };
        if overlap > 0.0 {
            let coverage = concept_coverage(node);
            let base = (if cue_set.is_empty() {
                overlap
            } else {
                (overlap + weight) / 2.0
            }) * coverage;
            let salience = SALIENCE_CAP
                .min(1.0 + ((node.salience.max(1.0) - 1.0) * SALIENCE_GAIN));
            term_seeds.insert(node.id.clone(), base * salience);
        }
        if vector_score >= params.vector_threshold {
            vector_seeds.insert(node.id.clone(), vector_score);
        }
    }

    let mut seeded: HashMap<String, f64> = term_seeds.clone();
    let mut vector_ranked: Vec<(String, f64)> = vector_seeds.into_iter().collect();
    vector_ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| compare_label(&labels, &a.0, &b.0))
    });
    for (id, score) in vector_ranked.iter().take(params.seed_top_m as usize) {
        let entry = seeded.entry(id.clone()).or_insert(0.0);
        *entry = (*entry).max(*score);
    }

    let mut ranked: Vec<(String, f64)> = seeded.iter().map(|(id, score)| (id.clone(), *score)).collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| compare_label(&labels, &a.0, &b.0))
    });
    ranked.truncate(CROSS_SCORE_TOP_N);
    if ranked.len() > 1 {
        let term_sets: Vec<HashSet<&str>> = ranked
            .iter()
            .map(|(id, _)| {
                nodes
                    .get(id)
                    .map(|node| node.terms.iter().map(String::as_str).collect())
                    .unwrap_or_default()
            })
            .collect();
        for index in 0..ranked.len() {
            let own = &term_sets[index];
            let mut support = 0.0;
            for peer in 0..ranked.len() {
                if peer == index {
                    continue;
                }
                let other = &term_sets[peer];
                if own.is_empty() || other.is_empty() {
                    continue;
                }
                let shared = own.iter().filter(|term| other.contains(*term)).count();
                support += (shared as f64 / ((own.len() * other.len()) as f64).sqrt())
                    * ranked[peer].1;
            }
            let (id, score) = ranked[index].clone();
            let blended = score * (1.0 - CROSS_SCORE_WEIGHT)
                + (support / (ranked.len() - 1) as f64) * CROSS_SCORE_WEIGHT;
            seeded.insert(id, blended);
        }
    }

    let mut adjacency: HashMap<String, Vec<Neighbor>> = HashMap::new();
    for edge in edges {
        if edge.valid_until.is_some() && !query.include_history {
            continue;
        }
        if matches!(edge.kind, EdgeKind::Supersede | EdgeKind::Inhibits) {
            adjacency
                .entry(edge.from.clone())
                .or_default()
                .push(Neighbor { to: edge.to.clone(), weight: edge.weight });
            continue;
        }
        adjacency
            .entry(edge.from.clone())
            .or_default()
            .push(Neighbor { to: edge.to.clone(), weight: edge.weight });
        adjacency
            .entry(edge.to.clone())
            .or_default()
            .push(Neighbor { to: edge.from.clone(), weight: edge.weight });
    }

    let mut scores: HashMap<String, f64> = seeded.clone();
    let mut trace: Vec<RecallTraceNode> = Vec::new();
    let mut seed_ranked: Vec<(String, f64)> =
        seeded.iter().map(|(id, score)| (id.clone(), *score)).collect();
    seed_ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| compare_label(&labels, &a.0, &b.0))
    });
    for (id, score) in &seed_ranked {
        trace.push(RecallTraceNode {
            id: id.clone(),
            kind: TraceNodeKind::Seed,
            score: *score,
            via_edge: None,
            truncated: false,
            salience: nodes.get(id).map(|node| node.salience),
        });
    }

    let mut candidate_scores: HashMap<String, f64> = HashMap::new();
    let mut candidate_from: HashMap<String, String> = HashMap::new();
    let mut frontier: Vec<String> = seeded.keys().cloned().collect();
    frontier.sort_by(|a, b| compare_label(&labels, a, b));
    let mut visited = seeded.len();
    let mut truncated = false;

    for hop in 1..=params.spread_hops {
        if frontier.is_empty() {
            break;
        }
        let k = (params.base_k - hop as f64 * params.k_decay).max(1.0) as usize;
        let mut candidates: HashMap<String, Neighbor> = HashMap::new();
        for id in &frontier {
            let base = scores.get(id).copied().unwrap_or(0.0);
            for neighbor in adjacency.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                if visited as i64 >= params.compute_budget {
                    truncated = true;
                    break;
                }
                visited += 1;
                let Some(target) = nodes.get(&neighbor.to) else {
                    continue;
                };
                if !is_active(target) {
                    continue;
                }
                let decayed = base * neighbor.weight * params.spread_decay;
                if decayed
                    > candidates
                        .get(&neighbor.to)
                        .map(|edge| edge.weight)
                        .unwrap_or(0.0)
                {
                    candidates.insert(
                        neighbor.to.clone(),
                        Neighbor {
                            to: neighbor.to.clone(),
                            weight: neighbor.weight,
                        },
                    );
                    candidate_scores.insert(neighbor.to.clone(), decayed);
                    candidate_from.insert(neighbor.to.clone(), id.clone());
                }
            }
            if truncated {
                break;
            }
        }
        let mut ordered: Vec<(String, f64)> = candidate_scores
            .iter()
            .map(|(id, score)| (id.clone(), *score))
            .collect();
        ordered.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| compare_label(&labels, &a.0, &b.0))
        });
        let winners: Vec<(String, f64)> = ordered.into_iter().take(k).collect();
        if winners.len() < candidates.len() {
            truncated = true;
        }
        frontier = Vec::new();
        for (id, score) in winners {
            let previous = scores.get(&id).copied().unwrap_or(0.0);
            scores.insert(id.clone(), previous.max(score));
            let from = candidate_from.get(&id).cloned().unwrap_or_default();
            let weight = adjacency
                .get(&from)
                .and_then(|edges| edges.iter().find(|edge| edge.to == id))
                .map(|edge| edge.weight)
                .unwrap_or(0.0);
            trace.push(RecallTraceNode {
                id: id.clone(),
                kind: TraceNodeKind::Spread,
                score,
                via_edge: Some(TraceEdge {
                    from,
                    weight,
                    decayed: score,
                }),
                truncated: false,
                salience: nodes.get(&id).map(|node| node.salience),
            });
            frontier.push(id);
        }
        candidate_scores.clear();
        candidate_from.clear();
        if truncated {
            break;
        }
    }

    let mut inhibition: HashMap<String, Vec<Neighbor>> = HashMap::new();
    for edge in edges {
        if edge.kind != EdgeKind::Inhibits {
            continue;
        }
        if edge.valid_until.is_some() && !query.include_history {
            continue;
        }
        inhibition
            .entry(edge.from.clone())
            .or_default()
            .push(Neighbor { to: edge.to.clone(), weight: edge.weight });
    }
    let score_snapshot: Vec<(String, f64)> =
        scores.iter().map(|(id, score)| (id.clone(), *score)).collect();
    for (id, score) in score_snapshot {
        for link in inhibition.get(&id).map(Vec::as_slice).unwrap_or(&[]) {
            let Some(target) = scores.get(&link.to).copied() else {
                continue;
            };
            let reduced = (target - link.weight * params.inhibition_factor * score).max(0.0);
            scores.insert(link.to.clone(), reduced);
            trace.push(RecallTraceNode {
                id: link.to.clone(),
                kind: TraceNodeKind::Inhibit,
                score: reduced,
                via_edge: Some(TraceEdge {
                    from: id.clone(),
                    weight: link.weight,
                    decayed: link.weight * params.inhibition_factor * score,
                }),
                truncated: false,
                salience: None,
            });
        }
    }

    let conflict_groups = build_conflict_groups(atoms, edges);
    let mut note_scores: HashMap<String, f64> = HashMap::new();
    for (id, score) in &scores {
        let Some(node) = nodes.get(id) else {
            continue;
        };
        if node.kind != NodeType::Atom {
            continue;
        }
        *note_scores.entry(node.note_id.clone()).or_insert(0.0) += *score;
    }
    let episode_filter = !note_scores.is_empty();
    let mut note_scope: HashMap<&str, &str> = HashMap::new();
    for atom in atoms {
        note_scope.entry(atom.note_id.as_str()).or_insert(atom.scope_key.as_str());
    }
    let mut by_scope: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();
    for (note, score) in &note_scores {
        let scope = note_scope.get(note.as_str()).copied().unwrap_or("");
        by_scope.entry(scope).or_default().push((note.as_str(), *score));
    }
    let mut top_notes: HashSet<String> = HashSet::new();
    for list in by_scope.values() {
        let mut ordered = list.clone();
        ordered.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| compare_label(&note_labels, a.0, b.0))
        });
        for (note, _) in ordered.iter().take(params.episode_top_n as usize) {
            top_notes.insert(note.to_string());
        }
    }

    let mut kept: Vec<(String, f64)> = scores
        .iter()
        .filter(|(_, score)| **score >= params.activation_threshold)
        .filter(|(id, _)| {
            let node = &nodes[*id];
            node.kind == NodeType::Concept || !episode_filter || top_notes.contains(&node.note_id)
        })
        .map(|(id, score)| (id.clone(), *score))
        .collect();
    kept.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| compare_label(&labels, &a.0, &b.0))
    });
    kept.truncate((query.limit.unwrap_or(params.top_k)).max(0) as usize);
    let items: Vec<RecallItem> = kept
        .into_iter()
        .map(|(id, score)| {
            let node = &nodes[&id];
            RecallItem {
                id,
                kind: node.kind,
                text: node.text.clone(),
                score,
                note_id: node.note_id.clone(),
                concept_id: node.concept_id.clone(),
                confidence: node.confidence,
                valid_from: node.valid_from,
                valid_until: node.valid_until,
                conflict_group: conflict_groups.get(&node.id).cloned(),
            }
        })
        .collect();

    if truncated {
        for node in &mut trace {
            node.truncated = true;
        }
    }
    RecallResult {
        items,
        trace,
        dropped: None,
    }
}

fn find_root(root: &str, parent: &HashMap<String, String>) -> String {
    let mut current = root.to_string();
    while let Some(next) = parent.get(&current)
        && next != &current
    {
        current = next.clone();
    }
    current
}

fn build_conflict_groups(atoms: &[Atom], edges: &[Edge]) -> HashMap<String, String> {
    let mut parent: HashMap<String, String> = atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom.id.clone()))
        .collect();
    for edge in edges {
        if edge.kind != EdgeKind::Supersede {
            continue;
        }
        if !parent.contains_key(&edge.from) || !parent.contains_key(&edge.to) {
            continue;
        }
        let root_a = find_root(&edge.from, &parent);
        let root_b = find_root(&edge.to, &parent);
        if root_a == root_b {
            continue;
        }
        if root_a.encode_utf16().cmp(root_b.encode_utf16()).is_lt() {
            parent.insert(root_b, root_a);
        } else {
            parent.insert(root_a, root_b);
        }
    }
    atoms
        .iter()
        .map(|atom| {
            let root = find_root(&atom.id, &parent);
            (atom.id.clone(), root)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{
        AssociativeMemoryParams, LabelSource, MemorySource, NodeType, TraceNodeKind as Kind,
    };

    fn params() -> AssociativeMemoryParams {
        AssociativeMemoryParams {
            max_atom_chars: 4096,
            context_window_ms: 1_000,
            base_learning_rate: 0.05,
            out_weight_budget: 4.0,
            max_edge_weight: 1.0,
            spread_hops: 2,
            spread_decay: 0.5,
            inhibition_factor: 0.5,
            base_k: 8.0,
            k_decay: 1.0,
            compute_budget: 100,
            seed_top_m: 5,
            episode_top_n: 8,
            list_limit: 20,
            top_k: 5,
            activation_threshold: 0.1,
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

    fn atom(id: &str, note: &str, terms: &[&str], weight: f64) -> Atom {
        let mut vector = serde_json::Map::new();
        for term in terms {
            vector.insert((*term).to_string(), serde_json::json!(weight));
        }
        Atom {
            id: id.into(),
            note_id: note.into(),
            scope_key: "scope".into(),
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
            created_at: 0,
            last_activated_at: 0,
        }
    }

    fn edge(from: &str, to: &str, kind: EdgeKind) -> Edge {
        Edge {
            id: format!("{from}>{to}"),
            from: from.into(),
            to: to.into(),
            kind,
            weight: 1.0,
            strengthen_count: 1,
            valid_from: 0,
            valid_until: None,
            created_at: 0,
            last_used_at: 0,
        }
    }

    fn query(cue: &str, include_history: bool) -> RecallQuery {
        RecallQuery {
            cue: cue.into(),
            limit: Some(5),
            max_tokens: None,
            include_history,
            scope: None,
            reinforce: None,
        }
    }

    #[test]
    fn a_cue_term_seeds_its_atom() {
        let atoms = vec![atom("a", "n1", &["snapshot"], 1.0)];
        let cfg = params();
        let result = activate(
            &query("snapshot", false),
            &atoms,
            &[],
            &[],
            &cfg,
            10,
            &[],
            None,
        );
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, "a");
        assert!(result.items[0].score > 0.0);
        assert_eq!(result.trace[0].kind, TraceNodeKind::Seed);
    }

    #[test]
    fn expired_atoms_are_hidden_unless_history_is_requested() {
        let mut expired = atom("a", "n1", &["snapshot"], 1.0);
        expired.valid_until = Some(5);
        let params = params();
        assert!(activate(&query("snapshot", false), &[expired.clone()], &[], &[], &params, 10, &[], None)
            .items
            .is_empty());
        assert_eq!(
            activate(&query("snapshot", true), &[expired.clone()], &[], &[], &params, 10, &[], None)
                .items
                .len(),
            1,
            "includeHistory surfaces expired nodes"
        );
        let mut deleted = expired.clone();
        deleted.valid_until = None;
        deleted.is_deleted = true;
        assert!(activate(&query("snapshot", true), &[deleted], &[], &[], &params, 10, &[], None)
            .items
            .is_empty());
    }

    #[test]
    fn spread_follows_hebbian_edges_and_stops_at_expired_nodes() {
        let atoms = vec![
            atom("a", "n1", &["snapshot"], 1.0),
            atom("b", "n2", &["snapshot"], 1.0),
        ];
        let edges = vec![edge("a", "b", EdgeKind::Hebbian)];
        let result = activate(
            &query("snapshot", false),
            &atoms,
            &[],
            &edges,
            &params(),
            10,
            &[],
            None,
        );
        assert_eq!(result.items.len(), 2);
        assert!(result
            .trace
            .iter()
            .any(|node| node.id == "b" && node.kind == TraceNodeKind::Spread));
    }

    #[test]
    fn supersede_edges_are_directed_and_build_conflict_groups() {
        let atoms = vec![
            atom("a", "n1", &["snapshot"], 1.0),
            atom("b", "n2", &["snapshot"], 1.0),
        ];
        let edges = vec![edge("a", "b", EdgeKind::Supersede)];
        let result = activate(
            &query("snapshot", false),
            &atoms,
            &[],
            &edges,
            &params(),
            10,
            &[],
            None,
        );
        assert_eq!(result.items.len(), 2);
        assert!(result.items.iter().all(|item| item.conflict_group.is_some()));
    }

    #[test]
    fn inhibition_reduces_the_target_and_leaves_a_trace() {
        let atoms = vec![
            atom("a", "n1", &["snapshot"], 1.0),
            atom("b", "n2", &["snapshot"], 1.0),
        ];
        let edges = vec![edge("a", "b", EdgeKind::Inhibits)];
        let result = activate(
            &query("snapshot", false),
            &atoms,
            &[],
            &edges,
            &params(),
            10,
            &[],
            None,
        );
        let inhibited = result
            .trace
            .iter()
            .find(|node| node.kind == TraceNodeKind::Inhibit)
            .expect("an inhibit trace node");
        assert_eq!(inhibited.id, "b");
        assert!(inhibited.score < 1.0);
    }

    #[test]
    fn the_activation_threshold_and_limit_bound_the_result() {
        let atoms = vec![
            atom("a", "n1", &["snapshot"], 1.0),
            atom("b", "n2", &["other"], 1.0),
        ];
        let mut high = params();
        high.activation_threshold = 100.0;
        assert!(activate(&query("snapshot", false), &atoms, &[], &[], &high, 10, &[], None)
            .items
            .is_empty());

        let mut limited = query("snapshot", false);
        limited.limit = Some(0);
        assert!(activate(&limited, &atoms, &[], &[], &params(), 10, &[], None)
            .items
            .is_empty());
    }

    #[test]
    fn a_procedure_is_offered_with_its_strength_as_salience() {
        let procedure = Procedure {
            id: "p1".into(),
            scope_key: "scope".into(),
            label: "build snapshot".into(),
            terms: vec!["snapshot".into()],
            vector: serde_json::Map::new(),
            step_ids: vec!["a".into()],
            source_note_id: "n1".into(),
            frequency: 8,
            pinned: false,
            suppressed: false,
            created_at: 0,
            last_activated_at: 0,
        };
        let result = activate(
            &query("snapshot", false),
            &[],
            &[],
            &[],
            &params(),
            10,
            &[procedure],
            None,
        );
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].kind, NodeType::Procedure);
        assert_eq!(result.trace[0].salience, Some(0.8));
        let _ = (LabelSource::DeterministicV1, MemorySource::User, Kind::Seed);
    }
}
