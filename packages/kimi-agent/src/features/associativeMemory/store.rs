use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::{IndexMap, IndexSet};

use serde::{Deserialize, Serialize};
use tokio::fs;

use super::types::{
    Atom, Cooccurrence, Concept, Edge, NoteRecord, Procedure, MemorySource,
};

const NOTE_PREFIX: &str = "note:";
const ATOM_PREFIX: &str = "atom:";
const CONCEPT_PREFIX: &str = "concept:";
const EDGE_PREFIX: &str = "edge:";
const COOC_PREFIX: &str = "cooc:";
const PROCEDURE_PREFIX: &str = "procedure:";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectMemory {
    pub atoms: Vec<Atom>,
    pub concepts: Vec<Concept>,
    pub edges: Vec<Edge>,
    pub notes: Vec<NoteRecord>,
    pub cooccurrences: Vec<Cooccurrence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedAtom {
    #[serde(default)]
    text: Option<String>,
    id: String,
    note_id: String,
    scope_key: String,
    node_type: super::types::NodeType,
    span_start: i64,
    span_end: i64,
    terms: Vec<String>,
    vector: serde_json::Map<String, serde_json::Value>,
    dense_vector: Option<Vec<f64>>,
    concept_id: Option<String>,
    base_weight: f64,
    confidence: f64,
    replay_priority: i64,
    pinned: bool,
    suppressed: bool,
    valid_from: i64,
    valid_until: Option<i64>,
    is_deleted: bool,
    created_at: i64,
    last_activated_at: i64,
}

impl From<&Atom> for PersistedAtom {
    fn from(atom: &Atom) -> Self {
        Self {
            text: None,
            id: atom.id.clone(),
            note_id: atom.note_id.clone(),
            scope_key: atom.scope_key.clone(),
            node_type: atom.node_type,
            span_start: atom.span_start,
            span_end: atom.span_end,
            terms: atom.terms.clone(),
            vector: atom.vector.clone(),
            dense_vector: atom.dense_vector.clone(),
            concept_id: atom.concept_id.clone(),
            base_weight: atom.base_weight,
            confidence: atom.confidence,
            replay_priority: atom.replay_priority,
            pinned: atom.pinned,
            suppressed: atom.suppressed,
            valid_from: atom.valid_from,
            valid_until: atom.valid_until,
            is_deleted: atom.is_deleted,
            created_at: atom.created_at,
            last_activated_at: atom.last_activated_at,
        }
    }
}

pub struct AssociativeMemoryStore {
    scope_key: String,
    dir: PathBuf,
    atoms: IndexMap<String, Atom>,
    concepts: IndexMap<String, Concept>,
    edges: IndexMap<String, Edge>,
    by_source: IndexMap<String, IndexSet<String>>,
    notes: IndexMap<String, NoteRecord>,
    coocs: IndexMap<String, Cooccurrence>,
    procedures: IndexMap<String, Procedure>,
}

impl AssociativeMemoryStore {
    pub async fn open(scope_key: impl Into<String>, store_root: &Path) -> Result<Self, String> {
        let scope_key = scope_key.into();
        let dir = store_root.join(&scope_key);
        fs::create_dir_all(&dir)
            .await
            .map_err(|error| format!("create memory store dir {}: {error}", dir.display()))?;
        let mut store = Self {
            scope_key,
            dir,
            atoms: IndexMap::new(),
            concepts: IndexMap::new(),
            edges: IndexMap::new(),
            by_source: IndexMap::new(),
            notes: IndexMap::new(),
            coocs: IndexMap::new(),
            procedures: IndexMap::new(),
        };
        store.load().await?;
        Ok(store)
    }

    pub fn scope_key(&self) -> &str {
        &self.scope_key
    }

    async fn load(&mut self) -> Result<(), String> {
        let mut paths = Vec::new();
        let mut entries = match fs::read_dir(&self.dir).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(format!("read memory store {}: {error}", self.dir.display()))
            }
        };
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|error| format!("read memory store {}: {error}", self.dir.display()))?
        {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                paths.push(path);
            }
        }
        let mut loaded = Vec::new();
        for path in paths {
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(raw) = fs::read_to_string(&path).await else {
                continue;
            };
            loaded.push((decode_key(stem), raw));
        }
        loaded.sort_by(|(left, _), (right, _)| {
            left.starts_with(NOTE_PREFIX)
                .cmp(&right.starts_with(NOTE_PREFIX))
                .reverse()
                .then_with(|| left.encode_utf16().cmp(right.encode_utf16()))
        });
        for (key, raw) in loaded {
            if let Some(id) = key.strip_prefix(NOTE_PREFIX) {
                if let Ok(stored) = serde_json::from_str::<serde_json::Value>(&raw) {
                    let Some(note_id) = stored.get("id").and_then(serde_json::Value::as_str) else {
                        continue;
                    };
                    let note = NoteRecord {
                        id: note_id.to_string(),
                        scope_key: stored
                            .get("scopeKey")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        text: stored
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        created_at: stored
                            .get("createdAt")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or_default(),
                        source: stored
                            .get("source")
                            .and_then(|value| serde_json::from_value(value.clone()).ok())
                            .unwrap_or(MemorySource::Legacy),
                    };
                    self.notes.insert(note.id.clone(), note);
                }
            } else if let Some(id) = key.strip_prefix(ATOM_PREFIX) {
                if let Ok(stored) = serde_json::from_str::<PersistedAtom>(&raw) {
                    let text = match self.notes.get(&stored.note_id) {
                        Some(note) => js_slice(&note.text, stored.span_start, stored.span_end),
                        None => stored.text.clone().unwrap_or_default(),
                    };
                    self.atoms.insert(
                        id.to_string(),
                        Atom {
                            id: stored.id,
                            note_id: stored.note_id,
                            scope_key: stored.scope_key,
                            node_type: stored.node_type,
                            text,
                            span_start: stored.span_start,
                            span_end: stored.span_end,
                            terms: stored.terms,
                            vector: stored.vector,
                            dense_vector: stored.dense_vector,
                            concept_id: stored.concept_id,
                            base_weight: stored.base_weight,
                            confidence: stored.confidence,
                            replay_priority: stored.replay_priority,
                            pinned: stored.pinned,
                            suppressed: stored.suppressed,
                            valid_from: stored.valid_from,
                            valid_until: stored.valid_until,
                            is_deleted: stored.is_deleted,
                            created_at: stored.created_at,
                            last_activated_at: stored.last_activated_at,
                        },
                    );
                }
            } else if let Some(id) = key.strip_prefix(CONCEPT_PREFIX) {
                if let Ok(concept) = serde_json::from_str::<Concept>(&raw) {
                    self.concepts.insert(concept.id.clone(), concept);
                }
            } else if let Some(id) = key.strip_prefix(EDGE_PREFIX) {
                if let Ok(edge) = serde_json::from_str::<Edge>(&raw) {
                    self.index_edge(&edge);
                    self.edges.insert(edge.id.clone(), edge);
                }
            } else if let Some(key) = key.strip_prefix(COOC_PREFIX)
                && let Ok(cooc) = serde_json::from_str::<Cooccurrence>(&raw)
            {
                self.coocs.insert(cooc.key.clone(), cooc);
            } else if let Some(id) = key.strip_prefix(PROCEDURE_PREFIX)
                && let Ok(procedure) = serde_json::from_str::<Procedure>(&raw)
            {
                self.procedures.insert(procedure.id.clone(), procedure);
            }
        }
        Ok(())
    }

    pub fn list_atoms(&self) -> Vec<Atom> {
        self.atoms.values().cloned().collect()
    }

    pub fn list_concepts(&self) -> Vec<Concept> {
        self.concepts.values().cloned().collect()
    }

    fn index_edge(&mut self, edge: &Edge) {
        self.by_source
            .entry(edge.from.clone())
            .or_default()
            .insert(edge.id.clone());
    }

    pub fn edges_from(&self, sources: &HashSet<String>) -> Vec<Edge> {
        let mut out = Vec::new();
        for source in sources {
            if let Some(ids) = self.by_source.get(source) {
                for id in ids {
                    if let Some(edge) = self.edges.get(id) {
                        out.push(edge.clone());
                    }
                }
            }
        }
        out
    }

    pub fn list_edges(&self) -> Vec<Edge> {
        self.edges.values().cloned().collect()
    }

    pub fn list_notes(&self) -> Vec<NoteRecord> {
        self.notes.values().cloned().collect()
    }

    pub fn list_cooccurrences(&self) -> Vec<Cooccurrence> {
        self.coocs.values().cloned().collect()
    }

    pub fn list_procedures(&self) -> Vec<Procedure> {
        self.procedures.values().cloned().collect()
    }

    pub fn get_procedure(&self, id: &str) -> Option<Procedure> {
        self.procedures.get(id).cloned()
    }

    pub fn get_atom(&self, id: &str) -> Option<Atom> {
        self.atoms.get(id).cloned()
    }

    pub fn get_edge(&self, id: &str) -> Option<Edge> {
        self.edges.get(id).cloned()
    }

    pub fn get_concept(&self, id: &str) -> Option<Concept> {
        self.concepts.get(id).cloned()
    }

    pub fn get_note(&self, id: &str) -> Option<NoteRecord> {
        self.notes.get(id).cloned()
    }

    pub fn get_cooccurrence(&self, key: &str) -> Option<Cooccurrence> {
        self.coocs.get(key).cloned()
    }

    pub fn snapshot(&self) -> ProjectMemory {
        ProjectMemory {
            atoms: self.list_atoms(),
            concepts: self.list_concepts(),
            edges: self.list_edges(),
            notes: self.list_notes(),
            cooccurrences: self.list_cooccurrences(),
        }
    }

    async fn write(&self, key: &str, value: &impl serde::Serialize) -> Result<(), String> {
        let path = self.dir.join(format!("{}.json", encode_key(key)));
        let body = serde_json::to_string(value)
            .map_err(|error| format!("encode {key}: {error}"))?;
        fs::write(&path, format!("{body}\n"))
            .await
            .map_err(|error| format!("write {}: {error}", path.display()))
    }

    async fn remove(&self, key: &str) -> Result<(), String> {
        let path = self.dir.join(format!("{}.json", encode_key(key)));
        match fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        }
    }

    pub async fn put_note(&mut self, note: NoteRecord) -> Result<(), String> {
        self.notes.insert(note.id.clone(), note.clone());
        self.write(&format!("{NOTE_PREFIX}{}", note.id), &note).await
    }

    pub async fn put_atom(&mut self, atom: Atom) -> Result<(), String> {
        self.atoms.insert(atom.id.clone(), atom.clone());
        self.write(&format!("{ATOM_PREFIX}{}", atom.id), &PersistedAtom::from(&atom))
            .await
    }

    pub async fn put_concept(&mut self, concept: Concept) -> Result<(), String> {
        self.concepts.insert(concept.id.clone(), concept.clone());
        self.write(&format!("{CONCEPT_PREFIX}{}", concept.id), &concept)
            .await
    }

    pub async fn put_edge(&mut self, edge: Edge) -> Result<(), String> {
        self.index_edge(&edge);
        self.edges.insert(edge.id.clone(), edge.clone());
        self.write(&format!("{EDGE_PREFIX}{}", edge.id), &edge).await
    }

    pub async fn put_cooccurrence(&mut self, cooc: Cooccurrence) -> Result<(), String> {
        self.coocs.insert(cooc.key.clone(), cooc.clone());
        self.write(&format!("{COOC_PREFIX}{}", cooc.key), &cooc).await
    }

    pub async fn put_procedure(&mut self, procedure: Procedure) -> Result<(), String> {
        self.procedures.insert(procedure.id.clone(), procedure.clone());
        self.write(&format!("{PROCEDURE_PREFIX}{}", procedure.id), &procedure)
            .await
    }

    pub async fn delete_atom(&mut self, id: &str) -> Result<(), String> {
        self.atoms.remove(id);
        self.remove(&format!("{ATOM_PREFIX}{id}")).await
    }

    pub async fn delete_edge(&mut self, id: &str) -> Result<(), String> {
        if let Some(existing) = self.edges.remove(id)
            && let Some(ids) = self.by_source.get_mut(&existing.from)
        {
            ids.remove(id);
        }
        self.remove(&format!("{EDGE_PREFIX}{id}")).await
    }

    pub async fn delete_note(&mut self, id: &str) -> Result<(), String> {
        self.notes.remove(id);
        self.remove(&format!("{NOTE_PREFIX}{id}")).await
    }

    pub async fn delete_concept(&mut self, id: &str) -> Result<(), String> {
        self.concepts.remove(id);
        self.remove(&format!("{CONCEPT_PREFIX}{id}")).await
    }

    pub async fn delete_cooccurrence(&mut self, key: &str) -> Result<(), String> {
        self.coocs.remove(key);
        self.remove(&format!("{COOC_PREFIX}{key}")).await
    }

    pub async fn delete_procedure(&mut self, id: &str) -> Result<(), String> {
        self.procedures.remove(id);
        self.remove(&format!("{PROCEDURE_PREFIX}{id}")).await
    }

    pub async fn clear_graph(&mut self) -> Result<(), String> {
        for key in self.atoms.keys().cloned().collect::<Vec<_>>() {
            self.remove(&format!("{ATOM_PREFIX}{key}")).await?;
        }
        for key in self.concepts.keys().cloned().collect::<Vec<_>>() {
            self.remove(&format!("{CONCEPT_PREFIX}{key}")).await?;
        }
        for key in self.edges.keys().cloned().collect::<Vec<_>>() {
            self.remove(&format!("{EDGE_PREFIX}{key}")).await?;
        }
        for key in self.coocs.keys().cloned().collect::<Vec<_>>() {
            self.remove(&format!("{COOC_PREFIX}{key}")).await?;
        }
        for key in self.procedures.keys().cloned().collect::<Vec<_>>() {
            self.remove(&format!("{PROCEDURE_PREFIX}{key}")).await?;
        }
        self.atoms.clear();
        self.concepts.clear();
        self.edges.clear();
        self.coocs.clear();
        self.procedures.clear();
        Ok(())
    }
}

fn js_slice(text: &str, start: i64, end: i64) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let length = units.len() as i64;
    let normalize = |value: i64| {
        if value < 0 {
            (length + value).max(0) as usize
        } else {
            value.min(length) as usize
        }
    };
    let start = normalize(start);
    let end = normalize(end);
    if end <= start {
        return String::new();
    }
    String::from_utf16_lossy(&units[start..end])
}

fn encode_key(key: &str) -> String {
    let mut out = String::new();
    for byte in key.bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn decode_key(encoded: &str) -> String {
    let bytes = encoded.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&encoded[index + 1..index + 3], 16)
        {
            out.push(byte);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::types::{
        EdgeKind, NodeType, WriteDecision,
    };

    async fn store() -> (tempfile::TempDir, AssociativeMemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = AssociativeMemoryStore::open("scope-a", dir.path())
            .await
            .unwrap();
        (dir, store)
    }

    fn atom(id: &str, note_id: &str, text: &str) -> Atom {
        Atom {
            id: id.into(),
            note_id: note_id.into(),
            scope_key: "scope-a".into(),
            node_type: NodeType::Atom,
            text: text.into(),
            span_start: 0,
            span_end: text.len() as i64,
            terms: vec!["term".into()],
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

    fn note(id: &str) -> NoteRecord {
        NoteRecord {
            id: id.into(),
            scope_key: "scope-a".into(),
            text: "body".into(),
            created_at: 1,
            source: MemorySource::User,
        }
    }

    #[tokio::test]
    async fn notes_and_atoms_round_trip_and_rebuild_the_span_text() {
        let (_dir, mut store) = store().await;
        store.put_note(note("note-1")).await.unwrap();
        store.put_atom(atom("atom-1", "note-1", "body")).await.unwrap();
        assert_eq!(store.get_atom("atom-1").unwrap().text, "body");

        let reopened = AssociativeMemoryStore::open("scope-a", _dir.path())
            .await
            .unwrap();
        assert_eq!(
            reopened.get_atom("atom-1").unwrap().text,
            "body",
            "the span text is rebuilt from the note on load"
        );
        assert_eq!(reopened.list_notes().len(), 1);
    }

    #[tokio::test]
    async fn an_atom_without_its_note_reloads_with_empty_text() {
        let (_dir, mut store) = store().await;
        store.put_atom(atom("atom-1", "note-1", "body")).await.unwrap();
        let reopened = AssociativeMemoryStore::open("scope-a", _dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.get_atom("atom-1").unwrap().text, "");
    }

    #[tokio::test]
    async fn edges_are_indexed_by_source_and_removed_from_the_index() {
        let (_dir, mut store) = store().await;
        store
            .put_edge(Edge {
                id: "edge-1".into(),
                from: "atom-1".into(),
                to: "atom-2".into(),
                kind: EdgeKind::Hebbian,
                weight: 0.5,
                strengthen_count: 1,
                valid_from: 0,
                valid_until: None,
                created_at: 1,
                last_used_at: 1,
            })
            .await
            .unwrap();
        let sources = HashSet::from(["atom-1".to_string()]);
        assert_eq!(store.edges_from(&sources).len(), 1);
        store.delete_edge("edge-1").await.unwrap();
        assert!(store.edges_from(&sources).is_empty());
    }

    #[tokio::test]
    async fn clear_graph_keeps_notes() {
        let (_dir, mut store) = store().await;
        store.put_note(note("note-1")).await.unwrap();
        store.put_atom(atom("atom-1", "note-1", "body")).await.unwrap();
        store.clear_graph().await.unwrap();
        assert!(store.list_atoms().is_empty());
        assert_eq!(store.list_notes().len(), 1, "notes survive clearGraph");
    }

    #[tokio::test]
    async fn cooccurrences_and_procedures_round_trip() {
        let (_dir, mut store) = store().await;
        store
            .put_cooccurrence(Cooccurrence {
                key: "a|b".into(),
                a: "atom-1".into(),
                b: "atom-2".into(),
                count: 3,
                last_at: 5,
            })
            .await
            .unwrap();
        store
            .put_procedure(Procedure {
                id: "proc-1".into(),
                scope_key: "scope-a".into(),
                label: "build".into(),
                terms: vec!["build".into()],
                vector: serde_json::Map::new(),
                step_ids: vec!["atom-1".into()],
                source_note_id: "note-1".into(),
                frequency: 2,
                pinned: false,
                suppressed: false,
                created_at: 1,
                last_activated_at: 0,
            })
            .await
            .unwrap();
        let reopened = AssociativeMemoryStore::open("scope-a", _dir.path())
            .await
            .unwrap();
        assert_eq!(reopened.get_cooccurrence("a|b").unwrap().count, 3);
        assert_eq!(reopened.get_procedure("proc-1").unwrap().frequency, 2);
    }

    #[test]
    fn decision_and_node_type_names_stay_stable() {
        assert_eq!(
            serde_json::to_string(&WriteDecision::Updated).unwrap(),
            "\"updated\""
        );
        assert_eq!(NodeType::Procedure.as_str(), "procedure");
    }
}
