//! v2 `agent/toolRegistry/toolRegistryService.ts` + `tool/toolContract.ts`.
//!
//! A session keeps one active table and a set of pending same-name
//! replacements. A same-name registration whose definition signature changed
//! does **not** replace the active tool mid-session: it is held as `pending`
//! and `list()` keeps serving the original info, while `list_live()` reports
//! what a new session would see. This is the state machine behind #4719 /
//! #4720 / #4733's "pin the active schema" behavior.

use std::collections::BTreeMap;

use serde_json::Value;

/// Where a registered tool came from (v2 `ToolSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    Builtin,
    Mcp,
    Host,
    Plugin,
}

impl ToolSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Mcp => "mcp",
            Self::Host => "host",
            Self::Plugin => "plugin",
        }
    }
}

/// Whether the tool is advertised inline or disclosed on demand
/// (v2 `ToolDisclosure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolDisclosure {
    #[default]
    Inline,
    Deferred,
}

impl ToolDisclosure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Deferred => "deferred",
        }
    }
}

/// One registry entry's definition (v2 `ToolInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: ToolSource,
    pub disclosure: ToolDisclosure,
}

/// v2 `createEntry`: the executable tool plus the immutable info snapshot
/// taken at registration time. A same-name update replaces the executable
/// tool but `list()` keeps serving this info while a replacement is pending.
#[derive(Debug, Clone)]
struct RegistryEntry {
    tool: ToolDefinition,
    info: ToolDefinition,
}

/// Recursively sort object keys so two schemas with the same content hash the
/// same regardless of insertion order (v2 `canonicalToolValue`).
fn canonical_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_value).collect()),
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by_key(|(left, _)| *left);
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, item)| (key.clone(), canonical_value(item)))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// v2 `toolDefinitionSchemaSignature`: name + description + canonical schema.
/// An absent schema means the empty object v2 substitutes.
pub fn schema_signature(tool: &ToolDefinition) -> String {
    let parameters = if tool.input_schema.is_null() {
        serde_json::json!({ "type": "object", "properties": {} })
    } else {
        tool.input_schema.clone()
    };
    serde_json::to_string(&serde_json::json!([
        tool.name,
        tool.description,
        canonical_value(&parameters),
    ]))
    .unwrap_or_default()
}

/// v2 `toolDefinitionVersionSignature`: schema + source + disclosure.
pub fn version_signature(tool: &ToolDefinition) -> String {
    serde_json::to_string(&serde_json::json!([
        schema_signature(tool),
        tool.source.as_str(),
        tool.disclosure.as_str(),
    ]))
    .unwrap_or_default()
}

/// The outcome of one `register` call (v2 `ToolRegistration.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    Active,
    Pending,
}

/// The session's tool registry. `list` is what the running model sees;
/// `list_live` is what a freshly constructed session would register.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    active: BTreeMap<String, RegistryEntry>,
    pending: BTreeMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// v2 `register`: a first registration is active; an identical
    /// same-name re-registration is active with the original info kept; a
    /// changed definition becomes pending while the active tool stays pinned.
    pub fn register(&mut self, tool: ToolDefinition) -> RegistrationStatus {
        let name = tool.name.clone();
        match self.active.get(&name).cloned() {
            None => {
                self.pending.remove(&name);
                self.active.insert(
                    name,
                    RegistryEntry {
                        tool: tool.clone(),
                        info: tool,
                    },
                );
                RegistrationStatus::Active
            }
            Some(current) if version_signature(&current.info) == version_signature(&tool) => {
                self.pending.remove(&name);
                self.active.insert(
                    name,
                    RegistryEntry {
                        tool,
                        info: current.info,
                    },
                );
                RegistrationStatus::Active
            }
            Some(_) => {
                self.pending.insert(name, tool);
                RegistrationStatus::Pending
            }
        }
    }

    /// The active definitions. A name with a pending replacement serves the
    /// immutable info snapshot taken at its first registration (v2 `list`).
    pub fn list(&self) -> Vec<ToolDefinition> {
        self.active
            .values()
            .map(|entry| {
                if self.pending.contains_key(&entry.info.name) {
                    entry.info.clone()
                } else {
                    entry.tool.clone()
                }
            })
            .collect()
    }

    /// Active definitions overlaid with pending replacements (v2
    /// `listLive`): what a new session would see.
    pub fn list_live(&self) -> Vec<ToolDefinition> {
        let mut live: BTreeMap<String, ToolDefinition> = self
            .active
            .iter()
            .map(|(name, entry)| (name.clone(), entry.tool.clone()))
            .collect();
        for (name, tool) in &self.pending {
            live.insert(name.clone(), tool.clone());
        }
        live.into_values().collect()
    }

    /// Whether a name carries a pending same-name replacement.
    pub fn has_pending(&self, name: &str) -> bool {
        self.pending.contains_key(name)
    }

    /// v2 `ToolRegistration.dispose` / `unregisterMcpServer`.
    pub fn unregister(&mut self, name: &str) {
        self.pending.remove(name);
        self.active.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, description: &str, schema: Value) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: description.into(),
            input_schema: schema,
            source: ToolSource::Mcp,
            disclosure: ToolDisclosure::Inline,
        }
    }

    #[test]
    fn schema_signature_ignores_object_key_order() {
        let a = tool(
            "X",
            "d",
            serde_json::json!({ "a": 1, "b": { "z": 2, "y": 3 } }),
        );
        let b = tool(
            "X",
            "d",
            serde_json::json!({ "b": { "y": 3, "z": 2 }, "a": 1 }),
        );
        assert_eq!(schema_signature(&a), schema_signature(&b));
        assert_ne!(
            schema_signature(&a),
            schema_signature(&tool("X", "changed", serde_json::json!({})))
        );
    }

    #[test]
    fn a_changed_same_name_registration_stays_pending() {
        let mut registry = ToolRegistry::new();
        let old = tool("mcp__s__t", "old", serde_json::json!({ "type": "object" }));
        assert_eq!(registry.register(old.clone()), RegistrationStatus::Active);
        let new = tool("mcp__s__t", "new", serde_json::json!({ "type": "object" }));
        assert_eq!(registry.register(new.clone()), RegistrationStatus::Pending);
        assert!(registry.has_pending("mcp__s__t"));
        // The running session keeps the old definition; a new session sees
        // the replacement.
        let active = registry.list();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].description, "old");
        let live = registry.list_live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].description, "new");
    }

    #[test]
    fn an_identical_re_registration_is_active_and_idempotent() {
        let mut registry = ToolRegistry::new();
        let original = tool("X", "same", serde_json::json!({}));
        assert_eq!(
            registry.register(original.clone()),
            RegistrationStatus::Active
        );
        // A changed definition is held pending; re-registering the original
        // signature is active again and drops the pending replacement.
        assert_eq!(
            registry.register(tool("X", "changed", serde_json::json!({}))),
            RegistrationStatus::Pending
        );
        assert_eq!(
            registry.register(original.clone()),
            RegistrationStatus::Active
        );
        assert!(!registry.has_pending("X"));
        assert_eq!(registry.list()[0].description, "same");
        assert_eq!(registry.list_live()[0].description, "same");
    }

    #[test]
    fn version_signature_distinguishes_source_and_disclosure() {
        let base = tool("X", "d", serde_json::json!({}));
        let mut mcp_deferred = base.clone();
        mcp_deferred.disclosure = ToolDisclosure::Deferred;
        assert_ne!(version_signature(&base), version_signature(&mcp_deferred));
        let mut builtin = base.clone();
        builtin.source = ToolSource::Builtin;
        assert_ne!(version_signature(&base), version_signature(&builtin));
    }
}
