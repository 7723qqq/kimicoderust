use std::collections::{HashMap, HashSet};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::terms::compare_by_text;
use super::types::{
    Atom, AssociativeMemoryParams, Cooccurrence, Concept, Edge, EdgeKind, LabelSource, NodeType,
};
use super::vector::{average_dense, average_vector, cosine, top_terms};

const MERGE_READINESS: f64 = 0.5;
const CONCEPT_TERM_LIMIT: usize = 12;

#[derive(Debug, Clone, PartialEq)]
pub struct ClusterResult {
    pub concepts: Vec<Concept>,
    pub edges: Vec<Edge>,
}

pub fn cooc_cluster_threshold(params: &AssociativeMemoryParams) -> i64 {
    (params.cluster_cooc_threshold * 2.0 * (1.0 - params.concept_formation_rate))
        .round()
        .max(1.0) as i64
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

pub fn concept_id(atom_ids: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(atom_ids.join("\u{1}").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..24]
        .to_string()
}

fn sparse_to_map(vector: &serde_json::Map<String, Value>) -> HashMap<String, f64> {
    vector
        .iter()
        .filter_map(|(term, value)| value.as_f64().map(|number| (term.clone(), number)))
        .collect()
}

fn concept_edge(concept: &Concept, atom_id: &str, now: i64) -> Edge {
    Edge {
        id: format!("{}>{}", concept.id, atom_id),
        from: concept.id.clone(),
        to: atom_id.to_string(),
        kind: EdgeKind::Concept,
        weight: 1.0,
        strengthen_count: 1,
        valid_from: now,
        valid_until: None,
        created_at: now,
        last_used_at: now,
    }
}

pub fn cluster_incremental(
    atoms: &[Atom],
    coocs: &[Cooccurrence],
    concepts: &[Concept],
    params: &AssociativeMemoryParams,
    now: i64,
) -> ClusterResult {
    let atom_by_id: HashMap<&str, &Atom> = atoms.iter().map(|atom| (atom.id.as_str(), atom)).collect();
    let mut parent: HashMap<String, String> = atoms
        .iter()
        .map(|atom| (atom.id.clone(), atom.id.clone()))
        .collect();
    let cooc_threshold = cooc_cluster_threshold(params);
    for cooc in coocs {
        if cooc.count < cooc_threshold {
            continue;
        }
        if !atom_by_id.contains_key(cooc.a.as_str()) || !atom_by_id.contains_key(cooc.b.as_str()) {
            continue;
        }
        let root_a = find_root(&cooc.a, &parent);
        let root_b = find_root(&cooc.b, &parent);
        if root_a == root_b {
            continue;
        }
        if root_a.encode_utf16().cmp(root_b.encode_utf16()).is_lt() {
            parent.insert(root_b, root_a);
        } else {
            parent.insert(root_a, root_b);
        }
    }

    let mut components: HashMap<String, Vec<String>> = HashMap::new();
    for atom in atoms {
        let root = find_root(&atom.id, &parent);
        components.entry(root).or_default().push(atom.id.clone());
    }

    let mut working: Vec<Concept> = concepts.to_vec();
    let mut sorted_components: Vec<&Vec<String>> = components.values().collect();
    sorted_components.sort_by(|a, b| {
        let left = atom_by_id.get(a[0].as_str());
        let right = atom_by_id.get(b[0].as_str());
        match (left, right) {
            (Some(left), Some(right)) => compare_by_text(
                (left.id.as_str(), left.text.as_str()),
                (right.id.as_str(), right.text.as_str()),
            ),
            _ => std::cmp::Ordering::Equal,
        }
    });

    for ids in sorted_components {
        if ids.len() < params.cluster_min_size as usize {
            continue;
        }
        for chunk in chunk_atoms(ids, params.cluster_max_size as usize, &atom_by_id) {
            let members: Vec<&Atom> = chunk
                .iter()
                .filter_map(|id| atom_by_id.get(id.as_str()).copied())
                .collect();
            let terms: HashSet<String> = top_terms(
                &average_vector(
                    &members
                        .iter()
                        .map(|member| sparse_to_map(&member.vector))
                        .collect::<Vec<_>>(),
                ),
                5,
            )
            .into_iter()
            .collect();
            let mut candidates: Vec<&Concept> = working
                .iter()
                .filter(|concept| {
                    concept.atom_ids.len() + chunk.len() <= params.cluster_max_size as usize
                })
                .filter(|concept| concept.readiness >= MERGE_READINESS)
                .filter(|concept| concept.terms.iter().any(|term| terms.contains(term)))
                .collect();
            candidates.sort_by(|a, b| {
                b.readiness
                    .partial_cmp(&a.readiness)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.label.encode_utf16().cmp(b.label.encode_utf16()))
                    .then_with(|| a.id.encode_utf16().cmp(b.id.encode_utf16()))
            });
            let chosen = candidates.first().copied();
            let concept = match chosen {
                None => build_concept(&chunk, &atom_by_id, params, now),
                Some(candidate) => merge_concept(candidate, &chunk, &atom_by_id, params),
            };
            if chosen.is_none() {
                working.push(concept);
            } else {
                let index = working.iter().position(|item| item.id == concept.id).unwrap();
                working[index] = concept;
            }
        }
    }

    let deduped = merge_overlapping(&working, &atom_by_id, params);
    let final_edges: Vec<Edge> = deduped
        .iter()
        .flat_map(|concept| {
            concept
                .atom_ids
                .iter()
                .map(|atom_id| concept_edge(concept, atom_id, now))
                .collect::<Vec<_>>()
        })
        .collect();
    let all = deduped
        .into_iter()
        .map(|concept| {
            let load = concept.atom_ids.len() as f64;
            let readiness = 1.0 / (1.0 + load / params.load_ref);
            Concept {
                load,
                readiness,
                ..concept
            }
        })
        .collect();
    ClusterResult {
        concepts: all,
        edges: final_edges,
    }
}

fn merge_overlapping(
    concepts: &[Concept],
    atom_by_id: &HashMap<&str, &Atom>,
    params: &AssociativeMemoryParams,
) -> Vec<Concept> {
    let mut merged: Vec<Concept> = Vec::new();
    for concept in concepts {
        let owner = merged.iter().position(|existing| {
            existing
                .atom_ids
                .iter()
                .any(|id| concept.atom_ids.contains(id))
        });
        let Some(owner_index) = owner else {
            merged.push(concept.clone());
            continue;
        };
        let extra: Vec<String> = concept
            .atom_ids
            .iter()
            .filter(|id| !merged[owner_index].atom_ids.contains(id))
            .cloned()
            .collect();
        if extra.is_empty() {
            continue;
        }
        let room = (params.cluster_max_size as usize).saturating_sub(merged[owner_index].atom_ids.len());
        if room == 0 {
            continue;
        }
        let owner = merged[owner_index].clone();
        merged[owner_index] = merge_concept(&owner, &extra[..room.min(extra.len())], atom_by_id, params);
    }
    merged
}

fn merge_concept(
    candidate: &Concept,
    chunk: &[String],
    atom_by_id: &HashMap<&str, &Atom>,
    params: &AssociativeMemoryParams,
) -> Concept {
    let merged_ids: Vec<String> = candidate
        .atom_ids
        .iter()
        .chain(chunk.iter())
        .filter(|id| atom_by_id.contains_key(id.as_str()))
        .cloned()
        .collect();
    let members: Vec<&Atom> = merged_ids
        .iter()
        .filter_map(|id| atom_by_id.get(id.as_str()).copied())
        .collect();
    let hub_atom_id = members
        .iter()
        .reduce(|best, atom| if atom.base_weight > best.base_weight { atom } else { best })
        .map(|atom| atom.id.clone());
    let dense_vectors = members
        .iter()
        .filter_map(|atom| atom.dense_vector.as_ref())
        .cloned()
        .collect::<Vec<_>>();
    let merged_len = merged_ids.len() as f64;
    Concept {
        label: label_from_atoms(&members),
        terms: top_terms(
            &average_vector(
                &members
                    .iter()
                    .map(|member| sparse_to_map(&member.vector))
                    .collect::<Vec<_>>(),
            ),
            CONCEPT_TERM_LIMIT,
        ),
        vector: None,
        hub_atom_id,
        dense_vector: average_dense(&dense_vectors),
        atom_ids: merged_ids.clone(),
        load: merged_len,
        readiness: 1.0 / (1.0 + merged_len / params.load_ref),
        ..candidate.clone()
    }
}

fn sort_utf16(values: &mut [String]) {
    values.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
}

fn chunk_atoms(
    ids: &[String],
    max: usize,
    atom_by_id: &HashMap<&str, &Atom>,
) -> Vec<Vec<String>> {
    if ids.len() <= max {
        let mut sorted = ids.to_vec();
        sort_utf16(&mut sorted);
        return vec![sorted];
    }
    let mut remaining: HashSet<String> = ids.iter().cloned().collect();
    let mut chunks = Vec::new();
    while !remaining.is_empty() {
        let mut seed = remaining.iter().cloned().collect::<Vec<_>>();
        sort_utf16(&mut seed);
        let seed = seed[0].clone();
        remaining.remove(&seed);
        let mut chunk = vec![seed.clone()];
        let mut centroid = sparse_to_map(&atom_by_id[seed.as_str()].vector);
        while chunk.len() < max && !remaining.is_empty() {
            let mut ordered = remaining.iter().cloned().collect::<Vec<_>>();
            sort_utf16(&mut ordered);
            let mut best_id: Option<String> = None;
            let mut best_score = -1.0;
            for id in ordered {
                let score = cosine(&centroid, &sparse_to_map(&atom_by_id[id.as_str()].vector));
                if score > best_score {
                    best_score = score;
                    best_id = Some(id);
                }
            }
            let Some(best_id) = best_id else {
                break;
            };
            chunk.push(best_id.clone());
            remaining.remove(&best_id);
            centroid = average_vector(&[
                centroid,
                sparse_to_map(&atom_by_id[best_id.as_str()].vector),
            ]);
        }
        sort_utf16(&mut chunk);
        chunks.push(chunk);
    }
    chunks
}

fn build_concept(
    atom_ids: &[String],
    atom_by_id: &HashMap<&str, &Atom>,
    params: &AssociativeMemoryParams,
    now: i64,
) -> Concept {
    let members: Vec<&Atom> = atom_ids
        .iter()
        .filter_map(|id| atom_by_id.get(id.as_str()).copied())
        .collect();
    let vector = average_vector(
        &members
            .iter()
            .map(|member| sparse_to_map(&member.vector))
            .collect::<Vec<_>>(),
    );
    let hub_atom_id = members
        .iter()
        .reduce(|best, atom| if atom.base_weight > best.base_weight { atom } else { best })
        .map(|atom| atom.id.clone());
    let dense_vectors = members
        .iter()
        .filter_map(|atom| atom.dense_vector.as_ref())
        .cloned()
        .collect::<Vec<_>>();
    let load = atom_ids.len() as f64;
    Concept {
        id: concept_id(atom_ids),
        scope_key: members[0].scope_key.clone(),
        node_type: NodeType::Concept,
        label: label_from_atoms(&members),
        label_source: LabelSource::DeterministicV1,
        terms: top_terms(&vector, CONCEPT_TERM_LIMIT),
        vector: None,
        hub_atom_id,
        dense_vector: average_dense(&dense_vectors),
        atom_ids: atom_ids.to_vec(),
        load,
        readiness: 1.0 / (1.0 + load / params.load_ref),
        pinned: false,
        suppressed: false,
        valid_from: now,
        valid_until: None,
        created_at: now,
        last_activated_at: now,
    }
}

fn label_from_atoms(atoms: &[&Atom]) -> String {
    let mut ranked = atoms.to_vec();
    ranked.sort_by(|a, b| {
        let left: f64 = a.vector.values().filter_map(Value::as_f64).sum();
        let right: f64 = b.vector.values().filter_map(Value::as_f64).sum();
        right
            .partial_cmp(&left)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                compare_by_text(
                    (a.id.as_str(), a.text.as_str()),
                    (b.id.as_str(), b.text.as_str()),
                )
            })
    });
    ranked.truncate(3);
    let mut terms: Vec<String> = Vec::new();
    for atom in ranked {
        for term in top_terms(&sparse_to_map(&atom.vector), 3) {
            if terms.len() >= 6 {
                break;
            }
            if !terms.contains(&term) {
                terms.push(term);
            }
        }
        if terms.len() >= 6 {
            break;
        }
    }
    terms.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::AssociativeMemoryParams;

    fn params(cluster_min_size: i64, cluster_max_size: i64) -> AssociativeMemoryParams {
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
            cluster_min_size,
            cluster_max_size,
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

    fn atom_with_terms(id: &str, terms: &[&str]) -> Atom {
        let mut vector = serde_json::Map::new();
        for term in terms {
            vector.insert((*term).to_string(), Value::from(1.0));
        }
        Atom {
            id: id.into(),
            note_id: "note-1".into(),
            scope_key: "scope-a".into(),
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

    #[test]
    fn the_cooc_threshold_matches_the_v2_formula() {
        let cfg = params(2, 8);
        assert_eq!(cooc_cluster_threshold(&cfg), 1, "0.5*2*(1-0.5)=0.5 rounds to 0, clamped to 1");
        let mut high = params(2, 8);
        high.cluster_cooc_threshold = 0.9;
        high.concept_formation_rate = 0.0;
        assert_eq!(cooc_cluster_threshold(&high), 2);
    }

    #[test]
    fn concept_ids_are_stable_and_salted_by_membership() {
        assert_eq!(
            concept_id(&["a".to_string(), "b".to_string()]),
            concept_id(&["a".to_string(), "b".to_string()])
        );
        assert_ne!(
            concept_id(&["a".to_string(), "b".to_string()]),
            concept_id(&["a".to_string(), "c".to_string()])
        );
        assert_eq!(
            concept_id(&["a".to_string()]).len(),
            24
        );
    }

    #[tokio::test]
    async fn clustering_links_atoms_through_strong_cooccurrences() {
        let atoms = vec![
            atom_with_terms("a1", &["alpha"]),
            atom_with_terms("a2", &["beta"]),
            atom_with_terms("a3", &["gamma"]),
        ];
        let coocs = vec![
            Cooccurrence {
                key: "alpha|beta".into(),
                a: "a1".into(),
                b: "a2".into(),
                count: 5,
                last_at: 1,
            },
            Cooccurrence {
                key: "beta|gamma".into(),
                a: "a2".into(),
                b: "a3".into(),
                count: 5,
                last_at: 1,
            },
        ];
        let result = cluster_incremental(&atoms, &coocs, &[], &params(2, 8), 10);
        assert_eq!(result.concepts.len(), 1, "all three link into one component");
        assert_eq!(result.concepts[0].atom_ids.len(), 3);
        assert_eq!(result.edges.len(), 3);
        assert!(result.concepts[0].label.contains("alpha"));
    }

    #[tokio::test]
    async fn a_component_below_the_min_size_is_skipped() {
        let atoms = vec![atom_with_terms("a", &["alpha"]), atom_with_terms("b", &["beta"])];
        let coocs = vec![Cooccurrence {
            key: "alpha|beta".into(),
            a: "a".into(),
            b: "b".into(),
            count: 5,
            last_at: 1,
        }];
        let params = params(3, 8);
        let result = cluster_incremental(&atoms, &coocs, &[], &params, 10);
        assert!(result.concepts.is_empty());
        assert!(result.edges.is_empty());
    }
}
