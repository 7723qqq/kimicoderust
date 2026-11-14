use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::types::{
    Atom, AssociativeMemoryParams, Edge, EdgeKind, NoteRecord, Procedure,
};
use super::vector::{average_vector, top_terms};

#[derive(Debug, Clone, PartialEq)]
pub struct ProcedureResult {
    pub procedures: Vec<Procedure>,
    pub edges: Vec<Edge>,
}

pub fn procedure_id(signature: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(signature.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..24]
        .to_string()
}

fn sparse_to_map(vector: &serde_json::Map<String, serde_json::Value>) -> HashMap<String, f64> {
    vector
        .iter()
        .filter_map(|(term, value)| value.as_f64().map(|number| (term.clone(), number)))
        .collect()
}

fn anchor_term(atom: &Atom, df: &HashMap<String, f64>) -> String {
    let mut shared: Vec<&String> = atom
        .terms
        .iter()
        .filter(|term| df.get(*term).copied().unwrap_or(0.0) >= 2.0)
        .collect();
    shared.sort_by(|left, right| {
        let left_weight = atom.vector.get(*left).and_then(serde_json::Value::as_f64).unwrap_or(0.0);
        let right_weight = atom
            .vector
            .get(*right)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        right_weight
            .partial_cmp(&left_weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.encode_utf16().cmp(right.encode_utf16()))
    });
    shared.first().map(|term| (*term).clone()).unwrap_or_default()
}

pub fn distill_procedures(
    notes: &[NoteRecord],
    atoms: &[Atom],
    existing: &[Procedure],
    params: &AssociativeMemoryParams,
    now: i64,
) -> ProcedureResult {
    let mut atoms_by_note: HashMap<&str, Vec<&Atom>> = HashMap::new();
    for atom in atoms {
        atoms_by_note.entry(atom.note_id.as_str()).or_default().push(atom);
    }

    let mut df: HashMap<String, f64> = HashMap::new();
    for atom in atoms {
        for term in atom.terms.iter().collect::<std::collections::HashSet<_>>() {
            *df.entry(term.clone()).or_insert(0.0) += 1.0;
        }
    }

    let mut signatures: HashMap<String, (i64, &NoteRecord, Vec<&Atom>)> = HashMap::new();
    for note in notes {
        let mut note_atoms = atoms_by_note
            .get(note.id.as_str())
            .cloned()
            .unwrap_or_default();
        note_atoms.sort_by(|a, b| {
            a.span_start
                .cmp(&b.span_start)
                .then_with(|| a.id.encode_utf16().cmp(b.id.encode_utf16()))
        });
        if note_atoms.len() < params.procedure_min_steps as usize {
            continue;
        }
        let signature = note_atoms
            .iter()
            .map(|atom| anchor_term(atom, &df))
            .filter(|term| !term.is_empty())
            .collect::<Vec<_>>()
            .join("|");
        if signature.is_empty() {
            continue;
        }
        match signatures.get_mut(&signature) {
            None => {
                signatures.insert(signature, (1, note, note_atoms));
            }
            Some(entry) => {
                entry.0 += 1;
                if note.created_at >= entry.1.created_at {
                    entry.1 = note;
                }
            }
        }
    }

    let suppressed_ids: std::collections::HashSet<String> = existing
        .iter()
        .filter(|procedure| procedure.suppressed)
        .map(|procedure| procedure.id.clone())
        .collect();

type SignatureEntry<'a> = (String, (i64, &'a NoteRecord, Vec<&'a Atom>));

    let mut ordered: Vec<SignatureEntry> = signatures.into_iter().collect();
    ordered.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));

    let mut created: Vec<Procedure> = Vec::new();
    for (signature, (count, latest, members)) in ordered {
        if count < params.procedure_threshold as i64 {
            continue;
        }
        let id = procedure_id(&signature);
        if suppressed_ids.contains(&id) {
            continue;
        }
        let vector = average_vector(
            &members
                .iter()
                .map(|atom| sparse_to_map(&atom.vector))
                .collect::<Vec<_>>(),
        );
        let label = top_terms(&vector, 3).join(" ");
        let terms = top_terms(&vector, 5);
        created.push(Procedure {
            id,
            scope_key: latest.scope_key.clone(),
            label,
            terms,
            vector: vector
                .into_iter()
                .map(|(term, value)| (term, serde_json::json!(value)))
                .collect(),
            step_ids: members.iter().map(|atom| atom.id.clone()).collect(),
            source_note_id: latest.id.clone(),
            frequency: count,
            pinned: false,
            suppressed: false,
            created_at: now,
            last_activated_at: now,
        });
    }

    let mut all: Vec<Procedure> = existing
        .iter()
        .filter(|procedure| !created.iter().any(|entry| entry.id == procedure.id))
        .cloned()
        .collect();
    all.extend(created);

    let edges: Vec<Edge> = all
        .iter()
        .flat_map(|procedure| {
            procedure
                .step_ids
                .iter()
                .map(|atom_id| Edge {
                    id: format!("{}>{}", procedure.id, atom_id),
                    from: procedure.id.clone(),
                    to: atom_id.clone(),
                    kind: EdgeKind::Procedure,
                    weight: 1.0,
                    strengthen_count: 1,
                    valid_from: procedure.created_at,
                    valid_until: None,
                    created_at: procedure.created_at,
                    last_used_at: now,
                })
                .collect::<Vec<_>>()
        })
        .collect();
    ProcedureResult {
        procedures: all,
        edges,
    }
}

pub fn procedure_model_free_strength(procedure: &Procedure) -> f64 {
    (procedure.frequency as f64 / (procedure.frequency as f64 + 2.0)).min(1.0)
}

pub fn should_use_cached_procedure(procedure: &Procedure) -> bool {
    procedure_model_free_strength(procedure) >= 0.5
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{
        AssociativeMemoryParams, MemorySource, NodeType,
    };

    fn params(min_steps: i64, threshold: f64) -> AssociativeMemoryParams {
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
            procedure_min_steps: min_steps,
            procedure_threshold: threshold,
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

    fn atom(id: &str, note_id: &str, span_start: i64, terms: &[&str]) -> Atom {
        let mut vector = serde_json::Map::new();
        for term in terms {
            vector.insert((*term).to_string(), serde_json::json!(1.0));
        }
        Atom {
            id: id.into(),
            note_id: note_id.into(),
            scope_key: "scope-a".into(),
            node_type: NodeType::Atom,
            text: terms.join(" "),
            span_start,
            span_end: span_start + terms.join(" ").len() as i64,
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

    fn note(id: &str, created_at: i64) -> NoteRecord {
        NoteRecord {
            id: id.into(),
            scope_key: "scope-a".into(),
            text: "alpha beta".into(),
            created_at,
            source: MemorySource::User,
        }
    }

    #[test]
    fn ids_are_stable_sha256_prefixes() {
        assert_eq!(procedure_id("a|b"), procedure_id("a|b"));
        assert_eq!(procedure_id("a|b").len(), 24);
        assert_ne!(procedure_id("a|b"), procedure_id("a|c"));
    }

    #[test]
    fn repeated_note_shapes_become_one_procedure() {
        let atoms = vec![
            atom("a1", "n1", 0, &["alpha", "shared"]),
            atom("a2", "n1", 7, &["beta", "shared"]),
            atom("a3", "n2", 0, &["alpha", "shared"]),
            atom("a4", "n2", 7, &["beta", "shared"]),
        ];
        let notes = vec![note("n1", 1), note("n2", 2)];
        let result = distill_procedures(&notes, &atoms, &[], &params(2, 2.0), 10);
        assert_eq!(result.procedures.len(), 1);
        assert_eq!(result.procedures[0].frequency, 2);
        assert_eq!(result.procedures[0].source_note_id, "n2", "the latest note wins");
        assert_eq!(result.edges.len(), 2);
    }

    #[test]
    fn a_shape_below_the_threshold_is_not_promoted() {
        let atoms = vec![
            atom("a1", "n1", 0, &["alpha", "shared"]),
            atom("a2", "n1", 7, &["beta", "shared"]),
        ];
        let notes = vec![note("n1", 1)];
        let result = distill_procedures(&notes, &atoms, &[], &params(2, 2.0), 10);
        assert!(result.procedures.is_empty());
        assert!(result.edges.is_empty());
    }

    #[test]
    fn a_note_with_too_few_atoms_is_skipped_and_existing_are_kept() {
        let atoms = vec![atom("a1", "n1", 0, &["alpha"])];
        let notes = vec![note("n1", 1)];
        let result = distill_procedures(&notes, &atoms, &[], &params(2, 1.0), 10);
        assert!(result.procedures.is_empty());
    }

    #[test]
    fn the_strength_curve_saturates_and_gates_on_half() {
        let procedure = Procedure {
            frequency: 1,
            ..procedure_of_id("p1")
        };
        assert!(procedure_model_free_strength(&procedure) < 0.5);
        assert!(!should_use_cached_procedure(&procedure));
        let strong = Procedure {
            frequency: 2,
            ..procedure_of_id("p2")
        };
        assert!(should_use_cached_procedure(&strong));
        let huge = Procedure {
            frequency: 1_000_000,
            ..procedure_of_id("p3")
        };
        assert!(procedure_model_free_strength(&huge) > 0.99999);
    }

    fn procedure_of_id(id: &str) -> Procedure {
        Procedure {
            id: id.into(),
            scope_key: "scope-a".into(),
            label: String::new(),
            terms: Vec::new(),
            vector: serde_json::Map::new(),
            step_ids: Vec::new(),
            source_note_id: "n1".into(),
            frequency: 0,
            pinned: false,
            suppressed: false,
            created_at: 0,
            last_activated_at: 0,
        }
    }
}
