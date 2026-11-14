use serde_json::{Value, json};

use crate::features::associativeMemory::params::associative_memory_params;
use crate::features::associativeMemory::service::AssociativeMemoryService;
use crate::features::associativeMemory::types::{
    ListOptions, MemoryScopeSelector, MemorySource, NodeType, RecallQuery, WriteDecision,
};
use crate::i18n::{LocalizedText, i18n_params};
use crate::tools::{err_result, ok_result};

pub const MEMORY_GRAPH_TOOL_NAME: &str = "MemoryGraph";

const MEMORY_GRAPH_DESCRIPTION: &str =
    include_str!("../features/associativeMemory/tools/memoryGraph.md");

pub fn memory_graph_description() -> String {
    MEMORY_GRAPH_DESCRIPTION.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryGraphAction {
    Write,
    Recall,
    Pin,
    Forget,
    List,
    Export,
    Replay,
    Propose,
    Approve,
    Label,
    Reconsolidate,
    Audit,
    Stats,
    Explain,
    Rebuild,
}

impl MemoryGraphAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Recall => "recall",
            Self::Pin => "pin",
            Self::Forget => "forget",
            Self::List => "list",
            Self::Export => "export",
            Self::Replay => "replay",
            Self::Propose => "propose",
            Self::Approve => "approve",
            Self::Label => "label",
            Self::Reconsolidate => "reconsolidate",
            Self::Audit => "audit",
            Self::Stats => "stats",
            Self::Explain => "explain",
            Self::Rebuild => "rebuild",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "write" => Self::Write,
            "recall" => Self::Recall,
            "pin" => Self::Pin,
            "forget" => Self::Forget,
            "list" => Self::List,
            "export" => Self::Export,
            "replay" => Self::Replay,
            "propose" => Self::Propose,
            "approve" => Self::Approve,
            "label" => Self::Label,
            "reconsolidate" => Self::Reconsolidate,
            "audit" => Self::Audit,
            "stats" => Self::Stats,
            "explain" => Self::Explain,
            "rebuild" => Self::Rebuild,
            _ => return None,
        })
    }
}

pub fn memory_graph_input_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": [
                    "write", "recall", "pin", "forget", "list", "export", "replay",
                    "propose", "approve", "label", "reconsolidate", "audit", "stats",
                    "explain", "rebuild"
                ],
                "description": "The memory graph operation to perform."
            },
            "text": {
                "type": "string",
                "description": "Note text (for `write`)."
            },
            "query": {
                "type": "string",
                "description": "Search cue (for `recall`)."
            },
            "id": {
                "type": "string",
                "description": "Atom or concept id (for `pin`, `forget`)."
            },
            "kind": {
                "type": "string",
                "enum": ["atom", "concept", "procedure"],
                "description": "Which kind to list (for `list`)."
            },
            "rule": {
                "type": "string",
                "description": "A JSON rule object (for `propose`)."
            },
            "index": {
                "type": "number",
                "description": "Proposal index to approve (for `approve`)."
            },
            "label": {
                "type": "string",
                "description": "Human-readable label (for `label`)."
            },
            "scope": {
                "type": "string",
                "enum": ["project", "global", "all"],
                "description": "Which memory scope to use. Defaults to `project` for writes, `all` for reads."
            },
            "limit": {
                "type": "number",
                "description": "Maximum results (for `recall`, `list`)."
            }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

fn parse_scope(value: Option<&Value>) -> Option<MemoryScopeSelector> {
    match value.and_then(Value::as_str) {
        Some("project") => Some(MemoryScopeSelector::Project),
        Some("global") => Some(MemoryScopeSelector::Global),
        Some("all") => Some(MemoryScopeSelector::All),
        _ => None,
    }
}

fn parse_kind(value: Option<&Value>) -> Option<NodeType> {
    match value.and_then(Value::as_str) {
        Some("atom") => Some(NodeType::Atom),
        Some("concept") => Some(NodeType::Concept),
        Some("procedure") => Some(NodeType::Procedure),
        _ => None,
    }
}

fn text_arg<'a>(args: &'a Value, field: &str) -> Option<&'a str> {
    args.get(field).and_then(Value::as_str)
}

fn required_text(args: &Value, key: &str) -> Result<String, String> {
    match text_arg(args, key) {
        Some(text) if !text.trim().is_empty() => Ok(text.to_string()),
        _ => Err(key.to_string()),
    }
}

pub async fn execute_memory_graph(
    service: &AssociativeMemoryService,
    args: &Value,
) -> crate::turn_loop::types::ExecutableToolResult {
    let Some(action) = text_arg(args, "action").and_then(MemoryGraphAction::parse) else {
        let raw = text_arg(args, "action").unwrap_or("");
        return err_result(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.unknownAction",
                i18n_params!["action" => raw],
            )
            .render(),
        );
    };
    match action {
        MemoryGraphAction::Write => handle_write(service, args).await,
        MemoryGraphAction::Recall => handle_recall(service, args).await,
        MemoryGraphAction::Pin => handle_pin(service, args).await,
        MemoryGraphAction::Forget => handle_forget(service, args).await,
        MemoryGraphAction::List => handle_list(service, args).await,
        MemoryGraphAction::Export => handle_export(service, args).await,
        MemoryGraphAction::Replay => handle_replay(service, args).await,
        MemoryGraphAction::Propose => handle_propose(service, args).await,
        MemoryGraphAction::Approve => handle_approve(service, args).await,
        MemoryGraphAction::Label => handle_label(service, args).await,
        MemoryGraphAction::Reconsolidate => handle_reconsolidate(service, args).await,
        MemoryGraphAction::Audit => handle_audit(service, args).await,
        MemoryGraphAction::Stats => handle_stats(service, args).await,
        MemoryGraphAction::Explain => handle_explain(service, args).await,
        MemoryGraphAction::Rebuild => handle_rebuild(service, args).await,
    }
}

async fn handle_write(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let text = match required_text(args, "text") {
        Ok(text) => text,
        Err(_) => {
            return err_result(LocalizedText::new("engine.tools.memoryGraph.writeRequired").render())
        }
    };
    if text_arg(args, "scope") == Some("all") {
        return err_result(
            LocalizedText::new("engine.tools.memoryGraph.scopeAllNotWritable").render(),
        );
    }
    let scope = if text_arg(args, "scope") == Some("global") {
        MemoryScopeSelector::Global
    } else {
        MemoryScopeSelector::Project
    };
    let result = match service.write_note(&text, scope, MemorySource::Tool).await {
        Ok(result) => result,
        Err(error) => return err_result(error),
    };
    if result.blocked_by_code {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.blockedByCode").render());
    }
    if result.decision == Some(WriteDecision::Noop) {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.alreadyRecorded").render());
    }
    ok_result(
        LocalizedText::with_params(
            "engine.tools.memoryGraph.wrote",
            i18n_params!["count" => result.atom_ids.len()],
        )
        .render(),
    )
}

async fn handle_recall(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let query = match required_text(args, "query") {
        Ok(query) => query,
        Err(_) => {
            return err_result(LocalizedText::new("engine.tools.memoryGraph.queryRequired").render())
        }
    };
    let params = associative_memory_params();
    let result = match service
        .recall(&RecallQuery {
            cue: query,
            limit: args.get("limit").and_then(Value::as_i64),
            max_tokens: Some(params.injection_max_tokens),
            include_history: false,
            scope: parse_scope(args.get("scope")),
            reinforce: Some(true),
        })
        .await
    {
        Ok(result) => result,
        Err(error) => return err_result(error),
    };
    if result.items.is_empty() {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.noResults").render());
    }
    let lines = result
        .items
        .iter()
        .map(|item| format!("- {} ({:.3}) [{}]", item.text, item.score, item.id))
        .collect::<Vec<_>>()
        .join("\n");
    ok_result(lines)
}

async fn handle_pin(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let Some(id) = text_arg(args, "id") else {
        return err_result(LocalizedText::new("engine.tools.memoryGraph.idRequired").render());
    };
    match service.pin(id).await {
        Ok(()) => ok_result(LocalizedText::new("engine.tools.memoryGraph.pinned").render()),
        Err(error) => err_result(error),
    }
}

async fn handle_forget(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let Some(id) = text_arg(args, "id") else {
        return err_result(LocalizedText::new("engine.tools.memoryGraph.idRequired").render());
    };
    match service.forget(id).await {
        Ok(()) => ok_result(LocalizedText::new("engine.tools.memoryGraph.forgot").render()),
        Err(error) => err_result(error),
    }
}

async fn handle_replay(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let result = match service
        .replay(args.get("limit").and_then(Value::as_i64))
        .await
    {
        Ok(result) => result,
        Err(error) => return err_result(error),
    };
    ok_result(
        LocalizedText::with_params(
            "engine.tools.memoryGraph.replayed",
            i18n_params!["count" => result.replayed],
        )
        .render(),
    )
}

async fn handle_propose(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let Some(rule) = text_arg(args, "rule") else {
        return err_result(LocalizedText::new("engine.tools.memoryGraph.ruleRequired").render());
    };
    let parsed: Value = match serde_json::from_str(rule) {
        Ok(parsed) => parsed,
        Err(_) => {
            return err_result(LocalizedText::new("engine.tools.memoryGraph.invalidRule").render())
        }
    };
    match service.propose_rule(&parsed).await {
        Ok(()) => ok_result(LocalizedText::new("engine.tools.memoryGraph.proposed").render()),
        Err(error) => err_result(error),
    }
}

async fn handle_reconsolidate(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let text = match required_text(args, "text") {
        Ok(text) => text,
        Err(_) => {
            return err_result(
                LocalizedText::new("engine.tools.memoryGraph.reconsolidateTextRequired").render(),
            )
        }
    };
    let id = text_arg(args, "id");
    match service.reconsolidate(id, &text).await {
        Ok(result) => ok_result(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.reconsolidated",
                i18n_params![
                    "count" => result.atom_ids.len(),
                    "superseded" => if result.superseded { "yes" } else { "no" }
                ],
            )
            .render(),
        ),
        Err(error) => err_result(error),
    }
}

async fn handle_audit(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let entries = match service
        .audit(args.get("limit").and_then(Value::as_i64))
        .await
    {
        Ok(entries) => entries,
        Err(error) => return err_result(error),
    };
    if entries.is_empty() {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.noAuditEntries").render());
    }
    let lines = entries
        .iter()
        .map(|entry| {
            let at = chrono::DateTime::from_timestamp_millis(entry.at)
                .map(|stamp| stamp.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
                .unwrap_or_default();
            format!("{at} {} {}", entry.kind.as_str(), Value::Object(entry.detail.clone()))
        })
        .collect::<Vec<_>>()
        .join("\n");
    ok_result(lines)
}

async fn handle_stats(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let stats = service.stats(parse_scope(args.get("scope"))).await;
    ok_result(
        LocalizedText::with_params(
            "engine.tools.memoryGraph.statsLine",
            i18n_params![
                "atoms" => stats.atoms,
                "concepts" => stats.concepts,
                "procedures" => stats.procedures,
                "edges" => stats.edges,
                "notes" => stats.notes,
                "pinned" => stats.pinned,
                "suppressed" => stats.suppressed
            ],
        )
        .render(),
    )
}

async fn handle_explain(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let query = match required_text(args, "query") {
        Ok(query) => query,
        Err(_) => {
            return err_result(LocalizedText::new("engine.tools.memoryGraph.queryRequired").render())
        }
    };
    let explained = match service
        .explain(&RecallQuery {
            cue: query,
            limit: None,
            max_tokens: None,
            include_history: false,
            scope: parse_scope(args.get("scope")),
            reinforce: Some(true),
        })
        .await
    {
        Ok(explained) => explained,
        Err(error) => return err_result(error),
    };
    if explained.trace.is_empty() && explained.dropped.is_empty() {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.noTrace").render());
    }
    let mut lines = explained
        .trace
        .iter()
        .map(|node| {
            format!(
                "{} {} {:.3}",
                node.kind.as_str(),
                node.id,
                node.score
            )
        })
        .collect::<Vec<_>>();
    for drop in &explained.dropped {
        lines.push(format!("- dropped {} by rule {}", drop.id, drop.rule));
    }
    ok_result(lines.join("\n"))
}

async fn handle_rebuild(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    match service.rebuild(parse_scope(args.get("scope"))).await {
        Ok(()) => ok_result(LocalizedText::new("engine.tools.memoryGraph.rebuilt").render()),
        Err(error) => err_result(error),
    }
}

async fn handle_label(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let (Some(id), Some(label)) = (text_arg(args, "id"), text_arg(args, "label")) else {
        return err_result(LocalizedText::new("engine.tools.memoryGraph.labelRequired").render());
    };
    match service.label_concept(id, label).await {
        Ok(()) => ok_result(LocalizedText::new("engine.tools.memoryGraph.labeled").render()),
        Err(error) => err_result(error),
    }
}

async fn handle_approve(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let Some(index) = args.get("index").and_then(Value::as_i64) else {
        return err_result(LocalizedText::new("engine.tools.memoryGraph.indexRequired").render());
    };
    match service.approve_proposal(index).await {
        Ok(_) => ok_result(LocalizedText::new("engine.tools.memoryGraph.approved").render()),
        Err(_) => err_result(LocalizedText::new("engine.tools.memoryGraph.approveFailed").render()),
    }
}

async fn handle_export(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let result = match service.export(parse_scope(args.get("scope"))).await {
        Ok(result) => result,
        Err(error) => return err_result(error),
    };
    if result.path.is_empty() {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.noMemories").render());
    }
    ok_result(
        LocalizedText::with_params(
            "engine.tools.memoryGraph.exported",
            i18n_params![
                "path" => result.path,
                "atoms" => result.atoms,
                "concepts" => result.concepts,
                "notes" => result.notes
            ],
        )
        .render(),
    )
}

async fn handle_list(service: &AssociativeMemoryService, args: &Value) -> crate::turn_loop::types::ExecutableToolResult {
    let result = match service
        .list(&ListOptions {
            kind: parse_kind(args.get("kind")),
            limit: args.get("limit").and_then(Value::as_i64),
            scope: parse_scope(args.get("scope")),
        })
        .await
    {
        Ok(result) => result,
        Err(error) => return err_result(error),
    };
    if result.atoms.is_empty() && result.concepts.is_empty() && result.procedures.is_empty() {
        return ok_result(LocalizedText::new("engine.tools.memoryGraph.noMemories").render());
    }
    let mut lines: Vec<String> = Vec::new();
    if !result.atoms.is_empty() {
        lines.push(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.listAtomsHeader",
                i18n_params!["count" => result.atoms.len()],
            )
            .render(),
        );
        for atom in &result.atoms {
            lines.push(format!("- [{}] {}", atom.id, atom.text));
        }
    }
    if !result.concepts.is_empty() {
        lines.push(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.listConceptsHeader",
                i18n_params!["count" => result.concepts.len()],
            )
            .render(),
        );
        for concept in &result.concepts {
            lines.push(format!("- [{}] {}", concept.id, concept.label));
        }
    }
    if !result.procedures.is_empty() {
        lines.push(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.listProceduresHeader",
                i18n_params!["count" => result.procedures.len()],
            )
            .render(),
        );
        for procedure in &result.procedures {
            lines.push(format!("- [{}] {}", procedure.id, procedure.label));
        }
    }
    ok_result(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::associativeMemory::service::{
        AssociativeMemoryService, MemoryDeps, MemoryFlags,
    };
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicI64;

    struct Flags(bool);
    impl MemoryFlags for Flags {
        fn enabled(&self, _id: &str) -> bool {
            self.0
        }
    }

    fn clock(start: i64) -> (Arc<AtomicI64>, Arc<dyn Fn() -> i64 + Send + Sync>) {
        let now = Arc::new(AtomicI64::new(start));
        let for_fn = now.clone();
        (now, Arc::new(move || for_fn.load(std::sync::atomic::Ordering::SeqCst)))
    }

    async fn service(dir: &Path) -> (Arc<AtomicI64>, AssociativeMemoryService) {
        let (now, clock) = clock(1_000);
        let service = AssociativeMemoryService::new(MemoryDeps {
            home_dir: dir.to_path_buf(),
            store_dir: dir.to_path_buf(),
            cwd: dir.to_path_buf(),
            session_id: "session-1".to_string(),
            session_cwd: dir.to_path_buf(),
            logger: None,
            flags: Some(Arc::new(Flags(true))),
            reminders: None,
            history: None,
            legacy_store: None,
        });
        (now, service.with_now_override(clock))
    }

    async fn run(
        service: &AssociativeMemoryService,
        args: Value,
    ) -> crate::turn_loop::types::ExecutableToolResult {
        execute_memory_graph(service, &args).await
    }

    #[test]
    fn the_schema_matches_v2_field_by_field() {
        let schema = memory_graph_input_schema();
        let actions = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            actions,
            vec![
                "write", "recall", "pin", "forget", "list", "export", "replay", "propose",
                "approve", "label", "reconsolidate", "audit", "stats", "explain", "rebuild"
            ]
        );
        for field in [
            "text",
            "query",
            "id",
            "kind",
            "rule",
            "index",
            "label",
            "scope",
            "limit",
        ] {
            assert!(schema["properties"].get(field).is_some(), "{field}");
        }
        assert_eq!(schema["required"], json!(["action"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        for (field, expected) in [
            ("kind", json!(["atom", "concept", "procedure"])),
            ("scope", json!(["project", "global", "all"])),
        ] {
            assert_eq!(schema["properties"][field]["enum"], expected);
        }
    }

    #[test]
    fn the_description_is_v2s_memory_graph_manual() {
        let description = memory_graph_description();
        assert!(description.starts_with(
            "Record experience into a self-training associative memory and recall it by association."
        ));
        for heading in [
            "### `write`",
            "### `recall`",
            "### `pin`",
            "### `forget`",
            "### `list`",
            "### `export`",
            "### `replay`",
            "### `propose` / `approve`",
            "### `label`",
            "### `reconsolidate`",
            "### `audit`",
            "### `stats`",
            "### `explain`",
            "### `rebuild`",
            "## Scopes",
            "## Procedures",
            "## When to use",
        ] {
            assert!(description.contains(heading), "{heading}");
        }
    }

    #[tokio::test]
    async fn an_unknown_action_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;
        let result = run(&service, json!({"action": "nope"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("nope"));
    }

    #[tokio::test]
    async fn every_action_requires_its_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;

        let cases = vec![
            (json!({"action": "write"}), "text"),
            (json!({"action": "write", "text": "  "}), "text"),
            (json!({"action": "write", "text": "x", "scope": "all"}), "all"),
            (json!({"action": "recall"}), "query"),
            (json!({"action": "pin"}), "id"),
            (json!({"action": "forget"}), "id"),
            (json!({"action": "propose"}), "rule"),
            (json!({"action": "propose", "rule": "not json"}), "JSON"),
            (json!({"action": "approve"}), "index"),
            (json!({"action": "label", "id": "a"}), "label"),
            (json!({"action": "reconsolidate"}), "text"),
            (json!({"action": "explain"}), "query"),
        ];
        for (args, expected) in cases {
            let label = args.to_string();
            let result = run(&service, args).await;
            assert!(result.is_error, "expected an error for {label}");
            assert!(
                result.content.contains(expected),
                "expected {expected:?} in {:?} for {label}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn write_recall_stats_and_audit_form_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;

        let written = run(
            &service,
            json!({"action": "write", "text": "the cache ttl is sixty seconds"}),
        )
        .await;
        assert!(!written.is_error, "{}", written.content);
        assert!(written.content.contains('1'), "{}", written.content);

        let recalled = run(
            &service,
            json!({"action": "recall", "query": "cache ttl", "limit": 5}),
        )
        .await;
        assert!(!recalled.is_error);
        assert!(recalled.content.contains("cache ttl"), "{}", recalled.content);
        assert!(recalled.content.contains('['), "the atom id is in brackets");

        let stats = run(&service, json!({"action": "stats", "scope": "all"})).await;
        assert!(!stats.is_error);
        for field in [
            "atoms", "concepts", "procedures", "edges", "notes", "pinned", "suppressed",
        ] {
            assert!(stats.content.contains(field), "{field}");
        }

        let audit = run(&service, json!({"action": "audit", "limit": 20})).await;
        assert!(!audit.is_error, "{}", audit.content);
        assert!(audit.content.contains("write_allowed"), "{}", audit.content);

        let explained = run(&service, json!({"action": "explain", "query": "cache ttl"})).await;
        assert!(!explained.is_error);
        assert!(explained.content.contains("seed"), "{}", explained.content);
    }

    #[tokio::test]
    async fn pin_and_forget_drive_the_list_output() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;
        run(
            &service,
            json!({"action": "write", "text": "the auth service retries twice"}),
        )
        .await;
        let listed = run(
            &service,
            json!({"action": "list", "kind": "atom", "limit": 10}),
        )
        .await;
        assert!(!listed.is_error, "{}", listed.content);
        assert!(listed.content.contains("Atoms (1)"), "{}", listed.content);

        let atom_id = listed
            .content
            .lines()
            .find_map(|line| line.split('[').nth(1))
            .and_then(|rest| rest.split(']').next())
            .unwrap()
            .to_string();

        let pinned = run(&service, json!({"action": "pin", "id": atom_id})).await;
        assert!(!pinned.is_error, "{}", pinned.content);

        let forgotten = run(&service, json!({"action": "forget", "id": atom_id})).await;
        assert!(!forgotten.is_error, "{}", forgotten.content);

        let after = run(&service, json!({"action": "stats"})).await;
        assert!(after.content.contains("atoms 0"), "{}", after.content);
        assert!(after.content.contains("suppressed 1"), "{}", after.content);
    }

    #[tokio::test]
    async fn export_rebuild_and_replay_report_their_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;
        run(
            &service,
            json!({"action": "write", "text": "alpha beta gamma"}),
        )
        .await;

        let exported = run(&service, json!({"action": "export", "scope": "all"})).await;
        assert!(!exported.is_error, "{}", exported.content);
        assert!(exported.content.contains("memory-export.json"), "{}", exported.content);
        let path = exported
            .content
            .rsplit("to ")
            .next()
            .unwrap()
            .trim()
            .to_string();
        assert!(Path::new(&path).exists());

        let replayed = run(&service, json!({"action": "replay", "limit": 5})).await;
        assert!(!replayed.is_error, "{}", replayed.content);
        assert!(replayed.content.contains('0'), "{}", replayed.content);

        let rebuilt = run(&service, json!({"action": "rebuild", "scope": "all"})).await;
        assert!(!rebuilt.is_error, "{}", rebuilt.content);

        let proposed = run(
            &service,
            json!({"action": "propose", "rule": "{\"id\":\"no-hosts\",\"pattern\":\"internal.example.com\",\"action\":\"block_write\",\"scope\":\"note\",\"reason\":\"internal host\"}"}),
        )
        .await;
        assert!(!proposed.is_error, "{}", proposed.content);

        let approved = run(&service, json!({"action": "approve", "index": 0})).await;
        assert!(!approved.is_error, "{}", approved.content);
    }

    #[tokio::test]
    async fn reconsolidate_targets_the_latest_recall_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;
        run(
            &service,
            json!({"action": "write", "text": "the auth service retries twice"}),
        )
        .await;
        run(&service, json!({"action": "recall", "query": "auth retries"})).await;
        let result = run(
            &service,
            json!({"action": "reconsolidate", "text": "the auth service retries twice before failing hard"}),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("superseded"), "{}", result.content);
    }

    #[tokio::test]
    async fn an_empty_store_reports_no_memories() {
        let dir = tempfile::tempdir().unwrap();
        let (_now, service) = service(dir.path()).await;
        let listed = run(&service, json!({"action": "list"})).await;
        assert!(!listed.is_error);
        assert_eq!(listed.content, "No memories yet.");
        let recalled = run(&service, json!({"action": "recall", "query": "nothing"})).await;
        assert_eq!(recalled.content, "No related memories.");
        let audited = run(&service, json!({"action": "audit"})).await;
        assert_eq!(audited.content, "No audit entries yet.");
        let exported = run(&service, json!({"action": "export"})).await;
        assert!(
            exported.content.starts_with("Exported 0 atoms, 0 concepts, 0 notes to "),
            "{}",
            exported.content
        );
        assert!(exported.content.ends_with("memory-export.json."), "{}", exported.content);
        let exported_path = exported
            .content
            .rsplit("to ")
            .next()
            .unwrap()
            .trim_end_matches('.')
            .to_string();
        assert!(Path::new(&exported_path).exists());
    }
}
