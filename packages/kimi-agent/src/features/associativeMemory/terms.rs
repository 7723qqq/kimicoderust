use std::collections::HashSet;

pub const PREFIX_MATCH_MIN: usize = 4;

pub fn stop_terms() -> &'static HashSet<&'static str> {
    static STOP_TERMS: std::sync::LazyLock<HashSet<&'static str>> =
        std::sync::LazyLock::new(|| {
            [
                "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "by", "as",
                "at", "is", "are", "was", "were", "be", "been", "being", "it", "its", "this",
                "that", "these", "those", "from", "into", "over", "after", "before", "than",
                "then", "so", "if", "but", "not", "no", "we", "you", "they", "he", "she", "i",
                "our", "their", "has", "have", "had",
            ]
            .into_iter()
            .collect()
        });
    &STOP_TERMS
}

pub fn is_stop_term(term: &str) -> bool {
    stop_terms().contains(term)
}

pub fn related_terms(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    let (shorter, longer) = if left.encode_utf16().count() <= right.encode_utf16().count() {
        (left, right)
    } else {
        (right, left)
    };
    shorter.encode_utf16().count() >= PREFIX_MATCH_MIN && longer.starts_with(shorter)
}

pub fn matches_any(term: &str, candidates: &[String]) -> bool {
    candidates.iter().any(|candidate| related_terms(term, candidate))
}

pub fn compare_by_text(left: (&str, &str), right: (&str, &str)) -> std::cmp::Ordering {
    let (left_id, left_text) = left;
    let (right_id, right_text) = right;
    left_text
        .encode_utf16()
        .cmp(right_text.encode_utf16())
        .then_with(|| left_id.encode_utf16().cmp(right_id.encode_utf16()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stop_list_matches_v2() {
        let terms = stop_terms();
        assert_eq!(terms.len(), 50);
        for term in ["the", "is", "we", "had", "from"] {
            assert!(terms.contains(term), "{term}");
        }
        assert!(!terms.contains("snapshot"));
        assert!(is_stop_term("the"));
        assert!(!is_stop_term("rust"));
    }

    #[test]
    fn prefix_matching_needs_four_characters() {
        assert!(related_terms("same", "same"));
        assert!(related_terms("snap", "snapshot"));
        assert!(related_terms("snapshot", "snap"));
        assert!(!related_terms("sna", "snapshot"), "shorter than PREFIX_MATCH_MIN");
        assert!(!related_terms("rust", "snapshot"));
        assert!(!related_terms("", "snapshot"));
    }

    #[test]
    fn matching_against_a_candidate_list() {
        let candidates = vec!["snapshot".to_string(), "graph".to_string()];
        assert!(matches_any("snap", &candidates));
        assert!(matches_any("graph", &candidates));
        assert!(!matches_any("edge", &candidates));
        assert!(!matches_any("snap", &[]));
    }

    #[test]
    fn text_ordering_breaks_ties_on_id() {
        assert_eq!(
            compare_by_text(("a", "same"), ("b", "same")),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            compare_by_text(("b", "same"), ("a", "same")),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            compare_by_text(("a", "same"), ("a", "same")),
            std::cmp::Ordering::Equal
        );
        assert_eq!(
            compare_by_text(("b", "aaa"), ("a", "zzz")),
            std::cmp::Ordering::Less
        );
    }
}
