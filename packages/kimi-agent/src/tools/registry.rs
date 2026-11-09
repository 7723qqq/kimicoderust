use std::collections::BTreeMap;

use serde_json::Value;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub source: ToolSource,
    pub disclosure: ToolDisclosure,
}

#[derive(Debug, Clone)]
struct RegistryEntry {
    tool: ToolDefinition,
    info: ToolDefinition,
}

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

pub fn version_signature(tool: &ToolDefinition) -> String {
    serde_json::to_string(&serde_json::json!([
        schema_signature(tool),
        tool.source.as_str(),
        tool.disclosure.as_str(),
    ]))
    .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationStatus {
    Active,
    Pending,
}

#[derive(Debug, Default)]
pub struct ToolRegistry {
    active: BTreeMap<String, RegistryEntry>,
    pending: BTreeMap<String, ToolDefinition>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

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

    pub fn has_pending(&self, name: &str) -> bool {
        self.pending.contains_key(name)
    }

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
