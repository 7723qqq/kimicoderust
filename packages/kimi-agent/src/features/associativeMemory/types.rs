use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeType {
    Atom,
    Concept,
    Procedure,
}

impl NodeType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Atom => "atom",
            Self::Concept => "concept",
            Self::Procedure => "procedure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Hebbian,
    Cooccurrence,
    Concept,
    Supersede,
    Inhibits,
    Procedure,
    Schema,
}

impl EdgeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hebbian => "hebbian",
            Self::Cooccurrence => "cooccurrence",
            Self::Concept => "concept",
            Self::Supersede => "supersede",
            Self::Inhibits => "inhibits",
            Self::Procedure => "procedure",
            Self::Schema => "schema",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LabelSource {
    DeterministicV1,
    Llm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySource {
    User,
    Tool,
    External,
    Legacy,
}

impl MemorySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Tool => "tool",
            Self::External => "external",
            Self::Legacy => "legacy",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemoryScopeSelector {
    Project,
    Global,
    All,
}

impl MemoryScopeSelector {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeAction {
    BlockWrite,
    BlockRecall,
    Pin,
    Supersede,
    BlockEmbed,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeScope {
    Note,
    Atom,
    Recall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteDecision {
    Added,
    Updated,
    Noop,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Atom {
    pub id: String,
    pub note_id: String,
    pub scope_key: String,
    pub node_type: NodeType,
    pub text: String,
    pub span_start: i64,
    pub span_end: i64,
    pub terms: Vec<String>,
    pub vector: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dense_vector: Option<Vec<f64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concept_id: Option<String>,
    pub base_weight: f64,
    pub confidence: f64,
    pub replay_priority: i64,
    pub pinned: bool,
    pub suppressed: bool,
    pub valid_from: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<i64>,
    pub is_deleted: bool,
    pub created_at: i64,
    pub last_activated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Concept {
    pub id: String,
    pub scope_key: String,
    pub node_type: NodeType,
    pub label: String,
    pub label_source: LabelSource,
    pub terms: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dense_vector: Option<Vec<f64>>,
    pub atom_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_atom_id: Option<String>,
    pub load: f64,
    pub readiness: f64,
    pub pinned: bool,
    pub suppressed: bool,
    pub valid_from: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<i64>,
    pub created_at: i64,
    pub last_activated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Procedure {
    pub id: String,
    pub scope_key: String,
    pub label: String,
    pub terms: Vec<String>,
    pub vector: serde_json::Map<String, Value>,
    pub step_ids: Vec<String>,
    pub source_note_id: String,
    pub frequency: i64,
    pub pinned: bool,
    pub suppressed: bool,
    pub created_at: i64,
    pub last_activated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Edge {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub weight: f64,
    pub strengthen_count: i64,
    pub valid_from: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<i64>,
    pub created_at: i64,
    pub last_used_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NoteRecord {
    pub id: String,
    pub scope_key: String,
    pub text: String,
    pub created_at: i64,
    pub source: MemorySource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cooccurrence {
    pub key: String,
    pub a: String,
    pub b: String,
    pub count: i64,
    pub last_at: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", rename_all_fields = "camelCase")]
pub enum MemoryEvent {
    Note {
        note_id: String,
        scope_key: String,
        text: String,
        at: i64,
    },
    Supersede {
        from_id: String,
        to_id: String,
        at: i64,
    },
    Forget {
        id: String,
        at: i64,
    },
    Pin {
        id: String,
        at: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteResult {
    pub note_id: String,
    pub atom_ids: Vec<String>,
    pub blocked_by_code: bool,
    pub scope: MemoryScopeSelector,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<WriteDecision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallQuery {
    pub cue: String,
    pub limit: Option<i64>,
    pub max_tokens: Option<i64>,
    pub include_history: bool,
    pub scope: Option<MemoryScopeSelector>,
    pub reinforce: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallItem {
    pub id: String,
    pub kind: NodeType,
    pub text: String,
    pub score: f64,
    pub note_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concept_id: Option<String>,
    pub confidence: f64,
    pub valid_from: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_until: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_group: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallTraceNode {
    pub id: String,
    pub kind: TraceNodeKind,
    pub score: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via_edge: Option<TraceEdge>,
    pub truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salience: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceNodeKind {
    Seed,
    Spread,
    Inhibit,
}

impl TraceNodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Seed => "seed",
            Self::Spread => "spread",
            Self::Inhibit => "inhibit",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceEdge {
    pub from: String,
    pub weight: f64,
    pub decayed: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallDrop {
    pub id: String,
    pub rule: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RecallResult {
    pub items: Vec<RecallItem>,
    pub trace: Vec<RecallTraceNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<Vec<RecallDrop>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociativeMemoryStats {
    pub atoms: i64,
    pub concepts: i64,
    pub procedures: i64,
    pub edges: i64,
    pub notes: i64,
    pub suppressed: i64,
    pub pinned: i64,
    pub mean_confidence: f64,
    pub working_items: i64,
    pub source_counts: serde_json::Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gating_mode: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociativeMemoryList {
    pub atoms: Vec<Atom>,
    pub concepts: Vec<Concept>,
    pub procedures: Vec<Procedure>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<NodeType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<MemoryScopeSelector>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociativeMemoryExport {
    pub path: String,
    pub atoms: i64,
    pub concepts: i64,
    pub notes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayResult {
    pub replayed: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconsolidateResult {
    pub note_id: String,
    pub atom_ids: Vec<String>,
    pub superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociativeMemoryParams {
    pub max_atom_chars: i64,
    pub context_window_ms: i64,
    pub base_learning_rate: f64,
    pub out_weight_budget: f64,
    pub max_edge_weight: f64,
    pub spread_hops: i64,
    pub spread_decay: f64,
    pub inhibition_factor: f64,
    pub base_k: f64,
    pub k_decay: f64,
    pub compute_budget: i64,
    pub seed_top_m: i64,
    pub episode_top_n: i64,
    pub list_limit: i64,
    pub top_k: i64,
    pub activation_threshold: f64,
    pub vector_threshold: f64,
    pub cluster_cooc_threshold: f64,
    pub cluster_min_size: i64,
    pub cluster_max_size: i64,
    pub procedure_min_steps: i64,
    pub procedure_threshold: f64,
    pub load_ref: f64,
    pub concept_formation_rate: f64,
    pub power_law_days: f64,
    pub half_life_ms: i64,
    pub prune_threshold: f64,
    pub injection_max_tokens: i64,
    pub supersede_threshold: f64,
    pub max_out_degree: i64,
    pub hetero_factor: f64,
    pub weight_dependence_k: f64,
    pub stdp_a_plus: f64,
    pub stdp_a_minus: f64,
    pub stdp_tau: f64,
    pub bcm_gain: f64,
    pub cascade_max_level: i64,
    pub cascade_strength: f64,
    pub sleep_downscale: f64,
    pub capture_count: i64,
    pub reconsolidate_merge_threshold: f64,
    pub reconsolidate_novel_threshold: f64,
    pub interference_forgetting: f64,
    pub replay_strata: i64,
    pub motivated_forgetting: f64,
    pub motivated_forgetting_ms: i64,
    pub schema_min_hops: i64,
    pub schema_top_k: i64,
    pub schema_ttl_ms: i64,
    pub interference_max_edges: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardRule {
    pub id: String,
    pub pattern: String,
    pub regex: bool,
    pub action: CodeAction,
    pub scope: CodeScope,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryCode {
    pub rules: Vec<HardRule>,
    pub guidance: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_and_edge_kinds_round_trip() {
        for kind in [
            NodeType::Atom,
            NodeType::Concept,
            NodeType::Procedure,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<NodeType>(&json).unwrap(), kind);
        }
        for kind in [
            EdgeKind::Hebbian,
            EdgeKind::Cooccurrence,
            EdgeKind::Concept,
            EdgeKind::Supersede,
            EdgeKind::Inhibits,
            EdgeKind::Procedure,
            EdgeKind::Schema,
        ] {
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<EdgeKind>(&json).unwrap(), kind);
        }
    }

    #[test]
    fn label_and_memory_sources_match_v2_names() {
        assert_eq!(serde_json::to_string(&LabelSource::DeterministicV1).unwrap(), "\"deterministic_v1\"");
        assert_eq!(serde_json::to_string(&LabelSource::Llm).unwrap(), "\"llm\"");
        assert_eq!(serde_json::to_string(&MemorySource::User).unwrap(), "\"user\"");
        assert_eq!(serde_json::to_string(&MemorySource::Legacy).unwrap(), "\"legacy\"");
        assert_eq!(serde_json::to_string(&CodeAction::BlockWrite).unwrap(), "\"block_write\"");
        assert_eq!(serde_json::to_string(&CodeScope::Recall).unwrap(), "\"recall\"");
        assert_eq!(serde_json::to_string(&WriteDecision::Blocked).unwrap(), "\"blocked\"");
    }

    #[test]
    fn memory_events_are_tagged_by_kind() {
        let event = MemoryEvent::Supersede {
            from_id: "a".into(),
            to_id: "b".into(),
            at: 7,
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["kind"], "supersede");
        assert_eq!(json["fromId"], "a");
        assert_eq!(
            serde_json::from_value::<MemoryEvent>(json).unwrap(),
            event
        );
    }

    #[test]
    fn the_param_block_carries_every_v2_field() {
        let params = AssociativeMemoryParams {
            max_atom_chars: 1,
            context_window_ms: 2,
            base_learning_rate: 3.0,
            out_weight_budget: 4.0,
            max_edge_weight: 5.0,
            spread_hops: 6,
            spread_decay: 7.0,
            inhibition_factor: 8.0,
            base_k: 9.0,
            k_decay: 10.0,
            compute_budget: 11,
            seed_top_m: 12,
            episode_top_n: 13,
            list_limit: 14,
            top_k: 15,
            activation_threshold: 16.0,
            vector_threshold: 17.0,
            cluster_cooc_threshold: 18.0,
            cluster_min_size: 19,
            cluster_max_size: 20,
            procedure_min_steps: 21,
            procedure_threshold: 22.0,
            load_ref: 23.0,
            concept_formation_rate: 24.0,
            power_law_days: 25.0,
            half_life_ms: 26,
            prune_threshold: 27.0,
            injection_max_tokens: 28,
            supersede_threshold: 29.0,
            max_out_degree: 30,
            hetero_factor: 31.0,
            weight_dependence_k: 32.0,
            stdp_a_plus: 33.0,
            stdp_a_minus: 34.0,
            stdp_tau: 35.0,
            bcm_gain: 36.0,
            cascade_max_level: 37,
            cascade_strength: 38.0,
            sleep_downscale: 39.0,
            capture_count: 40,
            reconsolidate_merge_threshold: 41.0,
            reconsolidate_novel_threshold: 42.0,
            interference_forgetting: 43.0,
            replay_strata: 44,
            motivated_forgetting: 45.0,
            motivated_forgetting_ms: 46,
            schema_min_hops: 47,
            schema_top_k: 48,
            schema_ttl_ms: 49,
            interference_max_edges: 50,
        };
        let json = serde_json::to_value(params).unwrap();
        for field in [
            "maxAtomChars",
            "contextWindowMs",
            "baseLearningRate",
            "outWeightBudget",
            "maxEdgeWeight",
            "spreadHops",
            "spreadDecay",
            "inhibitionFactor",
            "baseK",
            "kDecay",
            "computeBudget",
            "seedTopM",
            "episodeTopN",
            "listLimit",
            "topK",
            "activationThreshold",
            "vectorThreshold",
            "clusterCoocThreshold",
            "clusterMinSize",
            "clusterMaxSize",
            "procedureMinSteps",
            "procedureThreshold",
            "loadRef",
            "conceptFormationRate",
            "powerLawDays",
            "halfLifeMs",
            "pruneThreshold",
            "injectionMaxTokens",
            "supersedeThreshold",
            "maxOutDegree",
            "heteroFactor",
            "weightDependenceK",
            "stdpAPlus",
            "stdpAMinus",
            "stdpTau",
            "bcmGain",
            "cascadeMaxLevel",
            "cascadeStrength",
            "sleepDownscale",
            "captureCount",
            "reconsolidateMergeThreshold",
            "reconsolidateNovelThreshold",
            "interferenceForgetting",
            "replayStrata",
            "motivatedForgetting",
            "motivatedForgettingMs",
            "schemaMinHops",
            "schemaTopK",
            "schemaTtlMs",
            "interferenceMaxEdges",
        ] {
            assert!(json.get(field).is_some(), "missing {field}");
        }
    }
}
