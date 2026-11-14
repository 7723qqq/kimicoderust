use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::features::associativeMemory::params::associative_memory_params;
use crate::features::associativeMemory::service::AssociativeMemoryService;
use crate::features::associativeMemory::types::{
    ListOptions, MemoryScopeSelector, MemorySource, NodeType, RecallQuery, WriteDecision,
};
use crate::i18n::{LocalizedText, i18n_params};
use crate::turn_loop::types::{ExecutableToolResult, ToolInfo};

pub const MEMORY_GRAPH_TOOL_NAME: &str = "MemoryGraph";
pub const MEMORY_GRAPH_TOOL_SERVICE_ID: &str = "memoryGraphTool";
pub const MEMORY_GRAPH_DESCRIPTION: &str = include_str!("memoryGraph.md");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
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
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryGraphInput {
    pub action: MemoryGraphAction,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub kind: Option<NodeType>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub rule: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub index: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub scope: Option<MemoryScopeSelector>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_optional")]
    pub limit: Option<f64>,
}

pub fn memory_graph_input_schema() -> Value {
    json!({
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
            "text": { "type": "string", "description": "Note text (for `write`)." },
            "query": { "type": "string", "description": "Search cue (for `recall`)." },
            "id": { "type": "string", "description": "Atom or concept id (for `pin`, `forget`)." },
            "kind": {
                "type": "string",
                "enum": ["atom", "concept", "procedure"],
                "description": "Which kind to list (for `list`)."
            },
            "rule": { "type": "string", "description": "A JSON rule object (for `propose`)." },
            "index": { "type": "number", "description": "Proposal index to approve (for `approve`)." },
            "label": { "type": "string", "description": "Human-readable label (for `label`)." },
            "scope": {
                "type": "string",
                "enum": ["project", "global", "all"],
                "description": "Which memory scope to use. Defaults to `project` for writes, `all` for reads."
            },
            "limit": { "type": "number", "description": "Maximum results (for `recall`, `list`)." }
        },
        "required": ["action"],
        "additionalProperties": false
    })
}

pub struct MemoryGraphTool {
    memory: Arc<AssociativeMemoryService>,
}

pub struct MemoryGraphExecution<'a> {
    pub description: String,
    pub approval_rule: &'static str,
    tool: &'a MemoryGraphTool,
    input: MemoryGraphInput,
}

impl MemoryGraphExecution<'_> {
    pub async fn execute(self) -> Result<ExecutableToolResult, String> {
        self.tool.execute(&self.input).await
    }
}

impl MemoryGraphTool {
    pub fn new(memory: Arc<AssociativeMemoryService>) -> Self {
        Self { memory }
    }

    pub fn name(&self) -> &'static str {
        MEMORY_GRAPH_TOOL_NAME
    }

    pub fn side_effect(&self) -> &'static str {
        "write"
    }

    pub fn repo_mutation(&self) -> &'static str {
        "none"
    }

    pub fn description(&self) -> &'static str {
        MEMORY_GRAPH_DESCRIPTION
    }

    pub fn parameters(&self) -> Value {
        memory_graph_input_schema()
    }

    pub fn definition(&self) -> ToolInfo {
        ToolInfo {
            name: MEMORY_GRAPH_TOOL_NAME.to_string(),
            description: MEMORY_GRAPH_DESCRIPTION.to_string(),
            input_schema: memory_graph_input_schema(),
        }
    }

    pub fn resolve_execution(&self, input: MemoryGraphInput) -> MemoryGraphExecution<'_> {
        MemoryGraphExecution {
            description: format!("Memory graph: {}", input.action.as_str()),
            approval_rule: MEMORY_GRAPH_TOOL_NAME,
            tool: self,
            input,
        }
    }

    pub async fn execute(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        match args.action {
            MemoryGraphAction::Write => self.handle_write(args).await,
            MemoryGraphAction::Recall => self.handle_recall(args).await,
            MemoryGraphAction::Pin => self.handle_pin(args).await,
            MemoryGraphAction::Forget => self.handle_forget(args).await,
            MemoryGraphAction::List => self.handle_list(args).await,
            MemoryGraphAction::Export => self.handle_export(args).await,
            MemoryGraphAction::Replay => self.handle_replay(args).await,
            MemoryGraphAction::Propose => self.handle_propose(args).await,
            MemoryGraphAction::Approve => self.handle_approve(args).await,
            MemoryGraphAction::Label => self.handle_label(args).await,
            MemoryGraphAction::Reconsolidate => self.handle_reconsolidate(args).await,
            MemoryGraphAction::Audit => self.handle_audit(args).await,
            MemoryGraphAction::Stats => self.handle_stats(args).await,
            MemoryGraphAction::Explain => self.handle_explain(args).await,
            MemoryGraphAction::Rebuild => self.handle_rebuild(args).await,
        }
    }

    async fn handle_write(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(text) = args.text.as_deref().filter(|text| !trim_js(text).is_empty()) else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.writeRequired").render()));
        };
        if args.scope == Some(MemoryScopeSelector::All) {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.scopeAllNotWritable").render()));
        }
        let scope = if args.scope == Some(MemoryScopeSelector::Global) {
            MemoryScopeSelector::Global
        } else {
            MemoryScopeSelector::Project
        };
        let result = self.memory.write_note(text, scope, MemorySource::Tool).await?;
        if result.blocked_by_code {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.blockedByCode").render()));
        }
        if result.decision == Some(WriteDecision::Noop) {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.alreadyRecorded").render()));
        }
        Ok(ok_result(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.wrote",
                i18n_params!["count" => result.atom_ids.len()],
            )
            .render(),
        ))
    }

    async fn handle_recall(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(query) = args.query.as_deref().filter(|query| !trim_js(query).is_empty()) else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.queryRequired").render()));
        };
        let result = self.memory.recall(&RecallQuery {
            cue: query.to_string(),
            limit: args.limit.map(number_to_i64),
            max_tokens: Some(associative_memory_params().injection_max_tokens),
            include_history: false,
            scope: args.scope,
            reinforce: Some(true),
        }).await?;
        if result.items.is_empty() {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.noResults").render()));
        }
        Ok(ok_result(result.items.iter().map(|item| {
            format!("- {} ({}) [{}]", item.text, js_to_fixed(item.score), item.id)
        }).collect::<Vec<_>>().join("\n")))
    }

    async fn handle_pin(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(id) = args.id.as_deref() else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.idRequired").render()));
        };
        self.memory.pin(id).await?;
        Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.pinned").render()))
    }

    async fn handle_forget(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(id) = args.id.as_deref() else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.idRequired").render()));
        };
        self.memory.forget(id).await?;
        Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.forgot").render()))
    }

    async fn handle_replay(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let result = self.memory.replay(Some(args.limit.map(number_to_i64).unwrap_or(10))).await?;
        Ok(ok_result(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.replayed",
                i18n_params!["count" => result.replayed],
            )
            .render(),
        ))
    }

    async fn handle_propose(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(rule) = args.rule.as_deref() else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.ruleRequired").render()));
        };
        let parsed: Value = match serde_json::from_str(rule) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.invalidRule").render())),
        };
        self.memory.propose_rule(&parsed).await?;
        Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.proposed").render()))
    }

    async fn handle_reconsolidate(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(text) = args.text.as_deref().filter(|text| !trim_js(text).is_empty()) else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.reconsolidateTextRequired").render()));
        };
        let result = self.memory.reconsolidate(args.id.as_deref(), text).await?;
        Ok(ok_result(
            LocalizedText::with_params(
                "engine.tools.memoryGraph.reconsolidated",
                i18n_params![
                    "count" => result.atom_ids.len(),
                    "superseded" => if result.superseded { "yes" } else { "no" }
                ],
            )
            .render(),
        ))
    }

    async fn handle_audit(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let entries = self.memory.audit(Some(args.limit.map(number_to_i64).unwrap_or(20))).await?;
        if entries.is_empty() {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.noAuditEntries").render()));
        }
        let mut lines = Vec::new();
        for entry in entries {
            let at = js_iso_time(entry.at)?;
            lines.push(format!("{at} {} {}", entry.kind.as_str(), Value::Object(entry.detail)));
        }
        Ok(ok_result(lines.join("\n")))
    }

    async fn handle_stats(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let stats = self.memory.stats(args.scope).await;
        Ok(ok_result(
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
        ))
    }

    async fn handle_explain(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(query) = args.query.as_deref().filter(|query| !trim_js(query).is_empty()) else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.queryRequired").render()));
        };
        let explained = self.memory.explain(&RecallQuery {
            cue: query.to_string(),
            limit: None,
            max_tokens: None,
            include_history: false,
            scope: args.scope,
            reinforce: Some(true),
        }).await?;
        if explained.trace.is_empty() && explained.dropped.is_empty() {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.noTrace").render()));
        }
        let mut lines = explained.trace.iter().map(|node| {
            format!("{} {} {}", node.kind.as_str(), node.id, js_to_fixed(node.score))
        }).collect::<Vec<_>>();
        for drop in explained.dropped {
            lines.push(format!("- dropped {} by rule {}", drop.id, drop.rule));
        }
        Ok(ok_result(lines.join("\n")))
    }

    async fn handle_rebuild(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        self.memory.rebuild(Some(args.scope.unwrap_or(MemoryScopeSelector::All))).await?;
        Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.rebuilt").render()))
    }

    async fn handle_label(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let (Some(id), Some(label)) = (args.id.as_deref(), args.label.as_deref()) else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.labelRequired").render()));
        };
        self.memory.label_concept(id, label).await?;
        Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.labeled").render()))
    }

    async fn handle_approve(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let Some(index) = args.index else {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.indexRequired").render()));
        };
        if !index.is_finite() || index.fract() != 0.0 || index < 0.0 || index > i64::MAX as f64 {
            return Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.approveFailed").render()));
        }
        match self.memory.approve_proposal(index as i64).await {
            Ok(_) => Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.approved").render())),
            Err(_) => Ok(error_result(LocalizedText::new("engine.tools.memoryGraph.approveFailed").render())),
        }
    }

    async fn handle_export(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let result = self.memory.export(args.scope).await?;
        if result.path.is_empty() {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.noMemories").render()));
        }
        Ok(ok_result(
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
        ))
    }

    async fn handle_list(&self, args: &MemoryGraphInput) -> Result<ExecutableToolResult, String> {
        let result = self.memory.list(&ListOptions {
            kind: args.kind,
            limit: args.limit.map(number_to_i64),
            scope: args.scope,
        }).await?;
        if result.atoms.is_empty() && result.concepts.is_empty() && result.procedures.is_empty() {
            return Ok(ok_result(LocalizedText::new("engine.tools.memoryGraph.noMemories").render()));
        }
        let mut lines = Vec::new();
        if !result.atoms.is_empty() {
            lines.push(LocalizedText::with_params(
                "engine.tools.memoryGraph.listAtomsHeader",
                i18n_params!["count" => result.atoms.len()],
            ).render());
            for atom in result.atoms {
                lines.push(format!("- [{}] {}", atom.id, atom.text));
            }
        }
        if !result.concepts.is_empty() {
            lines.push(LocalizedText::with_params(
                "engine.tools.memoryGraph.listConceptsHeader",
                i18n_params!["count" => result.concepts.len()],
            ).render());
            for concept in result.concepts {
                lines.push(format!("- [{}] {}", concept.id, concept.label));
            }
        }
        if !result.procedures.is_empty() {
            lines.push(LocalizedText::with_params(
                "engine.tools.memoryGraph.listProceduresHeader",
                i18n_params!["count" => result.procedures.len()],
            ).render());
            for procedure in result.procedures {
                lines.push(format!("- [{}] {}", procedure.id, procedure.label));
            }
        }
        Ok(ok_result(lines.join("\n")))
    }
}

fn deserialize_optional<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn js_to_fixed(value: f64) -> String {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| ctx.eval::<String, _>(format!("({value}).toFixed(3)")))
            })
        })
        .unwrap_or_else(|_| format!("{value:.3}"))
}

fn js_iso_time(timestamp: i64) -> Result<String, String> {
    rquickjs::Runtime::new()
        .and_then(|runtime| {
            rquickjs::Context::full(&runtime).and_then(|context| {
                context.with(|ctx| {
                    ctx.eval::<String, _>(format!("new Date({timestamp}).toISOString()"))
                })
            })
        })
        .map_err(|_| "Invalid time value".to_string())
}

fn trim_js(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            | '\u{000a}'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{000d}'
            | '\u{0020}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'
            | '\u{2001}'
            | '\u{2002}'
            | '\u{2003}'
            | '\u{2004}'
            | '\u{2005}'
            | '\u{2006}'
            | '\u{2007}'
            | '\u{2008}'
            | '\u{2009}'
            | '\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
            | '\u{feff}'
    )
}

fn number_to_i64(value: f64) -> i64 {
    value as i64
}

fn ok_result(content: String) -> ExecutableToolResult {
    crate::tools::ok_result(content)
}

fn error_result(content: String) -> ExecutableToolResult {
    crate::tools::err_result(content)
}
