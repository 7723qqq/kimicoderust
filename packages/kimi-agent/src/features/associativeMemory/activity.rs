use std::collections::HashMap;

use super::types::{Edge, EdgeKind};

pub fn recent_activity(edges: &[Edge], now: i64, window_ms: i64) -> HashMap<String, f64> {
    let mut activity = HashMap::new();
    for edge in edges {
        if edge.kind != EdgeKind::Hebbian {
            continue;
        }
        if edge.last_used_at > now || now - edge.last_used_at > window_ms {
            continue;
        }
        *activity.entry(edge.from.clone()).or_insert(0.0) += edge.strengthen_count as f64;
    }
    activity
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::EdgeKind;

    fn edge(from: &str, kind: EdgeKind, last_used_at: i64, count: i64) -> Edge {
        Edge {
            id: format!("{from}-{last_used_at}"),
            from: from.into(),
            to: "b".into(),
            kind,
            weight: 1.0,
            strengthen_count: count,
            valid_from: 0,
            valid_until: None,
            created_at: 0,
            last_used_at,
        }
    }

    #[test]
    fn only_recent_hebbian_edges_count() {
        let edges = vec![
            edge("a", EdgeKind::Hebbian, 90, 2),
            edge("a", EdgeKind::Hebbian, 50, 3),
            edge("a", EdgeKind::Concept, 95, 9),
            edge("b", EdgeKind::Hebbian, 11, 5),
        ];
        let activity = recent_activity(&edges, 100, 80);
        assert_eq!(activity.get("a"), Some(&5.0));
        assert_eq!(activity.get("b"), None);
    }

    #[test]
    fn the_window_boundaries_match_v2() {
        let edges = vec![edge("a", EdgeKind::Hebbian, 20, 1)];
        assert!(recent_activity(&edges, 100, 80).contains_key("a"));
        assert!(!recent_activity(&edges, 101, 80).contains_key("a"));
        assert!(!recent_activity(&edges, 19, 80).contains_key("a"));
        assert!(recent_activity(&edges, 0, 80).is_empty());
    }
}
