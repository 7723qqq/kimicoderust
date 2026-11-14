use std::collections::{HashMap, HashSet};

use super::types::Atom;

pub fn compute_df(atoms: &[Atom]) -> HashMap<String, f64> {
    let mut df: HashMap<String, f64> = HashMap::new();
    for atom in atoms {
        for term in atom.terms.iter().collect::<HashSet<_>>() {
            *df.entry(term.clone()).or_insert(0.0) += 1.0;
        }
    }
    df
}

pub fn tfidf(
    terms: &[String],
    df: &HashMap<String, f64>,
    total_docs: i64,
) -> HashMap<String, f64> {
    if terms.is_empty() {
        return HashMap::new();
    }
    let mut counts: HashMap<String, f64> = HashMap::new();
    for term in terms {
        *counts.entry(term.clone()).or_insert(0.0) += 1.0;
    }
    let total = terms.len() as f64;
    counts
        .into_iter()
        .map(|(term, count)| {
            let idf = (((total_docs + 1) as f64) / (df.get(&term).copied().unwrap_or(0.0) + 1.0))
                .ln()
                + 1.0;
            (term, (count / total) * idf)
        })
        .collect()
}

pub fn term_frequency(terms: &[String]) -> HashMap<String, f64> {
    if terms.is_empty() {
        return HashMap::new();
    }
    let mut counts: HashMap<String, f64> = HashMap::new();
    for term in terms {
        *counts.entry(term.clone()).or_insert(0.0) += 1.0;
    }
    let total = terms.len() as f64;
    counts
        .into_iter()
        .map(|(term, count)| (term, count / total))
        .collect()
}

pub fn cosine(a: &HashMap<String, f64>, b: &HashMap<String, f64>) -> f64 {
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for (term, value) in a {
        norm_a += value * value;
        if let Some(other) = b.get(term) {
            dot += value * other;
        }
    }
    for value in b.values() {
        norm_b += value * value;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

pub fn average_vector(vectors: &[HashMap<String, f64>]) -> HashMap<String, f64> {
    if vectors.is_empty() {
        return HashMap::new();
    }
    let mut sum: HashMap<String, f64> = HashMap::new();
    for vector in vectors {
        for (term, value) in vector {
            *sum.entry(term.clone()).or_insert(0.0) += value;
        }
    }
    let count = vectors.len() as f64;
    sum.into_iter()
        .map(|(term, value)| (term, value / count))
        .collect()
}

pub fn average_dense(vectors: &[Vec<f64>]) -> Option<Vec<f64>> {
    if vectors.is_empty() {
        return None;
    }
    let length = vectors[0].len();
    if length == 0 || !vectors.iter().all(|vector| vector.len() == length) {
        return None;
    }
    let mut out = vec![0.0; length];
    let count = vectors.len() as f64;
    for vector in vectors {
        for (index, value) in vector.iter().enumerate() {
            out[index] += value / count;
        }
    }
    Some(out)
}

pub fn cosine_dense(a: &[f64], b: &[f64]) -> f64 {
    let length = a.len().min(b.len());
    let mut dot = 0.0;
    let mut norm_a = 0.0;
    let mut norm_b = 0.0;
    for index in 0..length {
        let x = a[index];
        let y = b[index];
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

pub fn top_terms(vector: &HashMap<String, f64>, limit: usize) -> Vec<String> {
    let mut entries: Vec<(&String, &f64)> = vector.iter().collect();
    entries.sort_by(|a, b| {
        b.1.partial_cmp(a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.encode_utf16().cmp(b.0.encode_utf16()))
    });
    entries
        .into_iter()
        .take(limit)
        .map(|(term, _)| term.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::NodeType;

    fn atom(id: &str, terms: &[&str]) -> Atom {
        Atom {
            id: id.into(),
            note_id: "note-1".into(),
            scope_key: "scope-a".into(),
            node_type: NodeType::Atom,
            text: "text".into(),
            span_start: 0,
            span_end: 4,
            terms: terms.iter().map(|term| term.to_string()).collect(),
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
            created_at: 1,
            last_activated_at: 0,
        }
    }

    #[test]
    fn df_counts_each_term_once_per_atom() {
        let atoms = vec![
            atom("a1", &["x", "y", "x"]),
            atom("a2", &["y", "z"]),
        ];
        let df = compute_df(&atoms);
        assert_eq!(df.get("x"), Some(&1.0));
        assert_eq!(df.get("y"), Some(&2.0));
        assert_eq!(df.get("z"), Some(&1.0));
        assert_eq!(compute_df(&[]).len(), 0);
    }

    #[test]
    fn tfidf_matches_the_v2_formula() {
        let terms = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let mut df = HashMap::new();
        df.insert("a".to_string(), 1.0);
        let vector = tfidf(&terms, &df, 1);
        let idf_a = ((2.0_f64) / (1.0 + 1.0)).ln() + 1.0;
        let idf_b = ((2.0_f64) / (0.0 + 1.0)).ln() + 1.0;
        assert_eq!(vector.get("a"), Some(&((2.0 / 3.0) * idf_a)));
        assert_eq!(vector.get("b"), Some(&((1.0 / 3.0) * idf_b)));
        assert!(tfidf(&[], &df, 1).is_empty());
    }

    #[test]
    fn term_frequency_is_a_share_of_the_term_count() {
        let terms = vec!["a".to_string(), "a".to_string(), "b".to_string()];
        let tf = term_frequency(&terms);
        assert_eq!(tf.get("a"), Some(&(2.0 / 3.0)));
        assert_eq!(tf.get("b"), Some(&(1.0 / 3.0)));
        assert!(term_frequency(&[]).is_empty());
    }

    #[test]
    fn cosine_is_one_for_parallel_vectors_and_zero_when_a_side_is_empty() {
        let mut a = HashMap::new();
        a.insert("x".to_string(), 1.0);
        a.insert("y".to_string(), 1.0);
        let mut b = HashMap::new();
        b.insert("x".to_string(), 2.0);
        b.insert("y".to_string(), 2.0);
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-12);
        assert_eq!(cosine(&a, &HashMap::new()), 0.0);
        assert_eq!(cosine(&HashMap::new(), &b), 0.0);
        let mut orthogonal = HashMap::new();
        orthogonal.insert("z".to_string(), 1.0);
        assert_eq!(cosine(&a, &orthogonal), 0.0);
    }

    #[test]
    fn average_vector_averages_per_term() {
        let mut first = HashMap::new();
        first.insert("x".to_string(), 1.0);
        let mut second = HashMap::new();
        second.insert("x".to_string(), 3.0);
        second.insert("y".to_string(), 2.0);
        let average = average_vector(&[first, second]);
        assert_eq!(average.get("x"), Some(&2.0));
        assert_eq!(average.get("y"), Some(&1.0));
        assert!(average_vector(&[]).is_empty());
    }

    #[test]
    fn average_dense_rejects_empty_and_ragged_inputs() {
        assert!(average_dense(&[]).is_none());
        assert!(average_dense(&[vec![]]).is_none());
        assert!(average_dense(&[vec![1.0], vec![1.0, 2.0]]).is_none());
        assert_eq!(average_dense(&[vec![1.0, 3.0], vec![3.0, 1.0]]), Some(vec![2.0, 2.0]));
    }

    #[test]
    fn dense_cosine_uses_the_shared_prefix() {
        assert!((cosine_dense(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-12);
        assert_eq!(cosine_dense(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine_dense(&[1.0, 0.0], &[1.0, 0.0, 5.0]), 1.0);
        assert_eq!(cosine_dense(&[], &[]), 0.0);
    }

    #[test]
    fn top_terms_orders_by_value_then_name() {
        let mut vector = HashMap::new();
        vector.insert("b".to_string(), 1.0);
        vector.insert("a".to_string(), 2.0);
        vector.insert("c".to_string(), 2.0);
        assert_eq!(top_terms(&vector, 3), vec!["a", "c", "b"]);
        assert_eq!(top_terms(&vector, 2), vec!["a", "c"]);
        assert!(top_terms(&vector, 0).is_empty());
    }
}
