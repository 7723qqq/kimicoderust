use sha2::{Digest, Sha256};

pub fn note_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = crate::tools::memory_search::tokenize(text)
        .into_iter()
        .filter(|term| term.len() <= 0xffff)
        .map(|term| term.to_lowercase())
        .collect();
    terms.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
    terms.dedup();
    terms
}

pub fn note_fingerprint(text: &str) -> String {
    let terms = note_terms(text);
    let mut hasher = Sha256::new();
    hasher.update(terms.join("\u{0}").as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..24]
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terms_are_deduplicated_and_sorted() {
        let terms = note_terms("Rust rust 记忆 rust");
        assert_eq!(terms, vec!["rust", "忆", "记", "记忆"]);
    }

    #[test]
    fn the_fingerprint_is_a_stable_sha256_prefix() {
        let first = note_fingerprint("A before B");
        assert_eq!(first.len(), 24);
        assert_eq!(first, note_fingerprint("A before B"));
        assert_eq!(
            first,
            note_fingerprint("B before A"),
            "terms compare as a set, so reordering collides"
        );
        assert_ne!(first, note_fingerprint("A before C"));
        assert_eq!(note_fingerprint(""), note_fingerprint(""));
    }
}
