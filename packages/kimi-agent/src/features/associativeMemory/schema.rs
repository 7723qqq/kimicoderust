use std::collections::{HashMap, HashSet};

use super::types::{AssociativeMemoryParams, RecallTraceNode, TraceNodeKind};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaLink {
    pub from: String,
    pub to: String,
    pub hops: i64,
}

pub fn collect_schema_links(
    trace: &[RecallTraceNode],
    params: &AssociativeMemoryParams,
) -> Vec<SchemaLink> {
    let mut parents: HashMap<String, String> = HashMap::new();
    let mut seeds: HashSet<String> = HashSet::new();
    for node in trace {
        match node.kind {
            TraceNodeKind::Seed => {
                seeds.insert(node.id.clone());
            }
            TraceNodeKind::Spread => {
                if let Some(edge) = &node.via_edge {
                    parents.insert(node.id.clone(), edge.from.clone());
                }
            }
            TraceNodeKind::Inhibit => {}
        }
    }
    let mut links: Vec<SchemaLink> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for node in trace {
        if node.kind != TraceNodeKind::Spread {
            continue;
        }
        let mut current = node.id.clone();
        let mut hops = 0_i64;
        while hops <= params.spread_hops + 1 {
            let Some(parent) = parents.get(&current) else {
                break;
            };
            hops += 1;
            if seeds.contains(parent) {
                let key = format!("{parent}?{}", node.id);
                if hops >= params.schema_min_hops && seen.insert(key) {
                    links.push(SchemaLink {
                        from: parent.clone(),
                        to: node.id.clone(),
                        hops,
                    });
                }
                break;
            }
            current = parent.clone();
        }
    }
    links.sort_by(|left, right| {
        left.hops
            .cmp(&right.hops)
            .then_with(|| left.to.encode_utf16().cmp(right.to.encode_utf16()))
    });
    links.truncate(params.schema_top_k.max(0) as usize);
    links
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::params::associative_memory_params;
    use crate::features::associativeMemory::types::TraceEdge;

    fn spread(id: &str, via: Option<&str>) -> RecallTraceNode {
        RecallTraceNode {
            id: id.into(),
            kind: TraceNodeKind::Spread,
            score: 1.0,
            via_edge: via.map(|from| TraceEdge {
                from: from.into(),
                weight: 1.0,
                decayed: 1.0,
            }),
            truncated: false,
            salience: None,
        }
    }

    fn seed(id: &str) -> RecallTraceNode {
        RecallTraceNode {
            id: id.into(),
            kind: TraceNodeKind::Seed,
            score: 1.0,
            via_edge: None,
            truncated: false,
            salience: None,
        }
    }

    #[test]
    fn links_walk_up_to_the_seed_and_stop_there() {
        let trace = vec![
            seed("s"),
            spread("a", Some("s")),
            spread("b", Some("a")),
            spread("c", Some("b")),
        ];
        let mut params = associative_memory_params();
        params.schema_min_hops = 1;
        let links = collect_schema_links(&trace, &params);
        assert_eq!(
            links
                .iter()
                .map(|link| (link.to.as_str(), link.hops))
                .collect::<Vec<_>>(),
            vec![("a", 1), ("b", 2), ("c", 3)]
        );
        assert!(links.iter().all(|link| link.from == "s"));
    }

    #[test]
    fn short_links_are_filtered_and_results_are_capped() {
        let trace = vec![seed("s"), spread("a", Some("s"))];
        let mut params = associative_memory_params();
        params.schema_min_hops = 3;
        assert!(collect_schema_links(&trace, &params).is_empty());

        let trace = vec![
            seed("s"),
            spread("a", Some("s")),
            spread("b", Some("a")),
            spread("c", Some("b")),
        ];
        params.schema_min_hops = 1;
        params.schema_top_k = 1;
        let links = collect_schema_links(&trace, &params);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].to, "a");
    }

    #[test]
    fn an_orphan_spread_node_produces_no_link() {
        let trace = vec![seed("s"), spread("a", None), spread("z", Some("a"))];
        assert!(collect_schema_links(&trace, &associative_memory_params()).is_empty());
    }

    #[test]
    fn the_walk_is_bounded_by_spread_hops_plus_one() {
        let trace = vec![
            seed("s"),
            spread("a", Some("s")),
            spread("b", Some("a")),
            spread("c", Some("b")),
            spread("d", Some("c")),
            spread("e", Some("d")),
        ];
        let mut params = associative_memory_params();
        params.spread_hops = 0;
        params.schema_min_hops = 1;
        let links = collect_schema_links(&trace, &params);
        assert_eq!(
            links
                .iter()
                .map(|link| (link.to.as_str(), link.hops))
                .collect::<Vec<_>>(),
            vec![("a", 1), ("b", 2)],
            "the walk stops after spread_hops + 1 parent steps"
        );
    }
}
