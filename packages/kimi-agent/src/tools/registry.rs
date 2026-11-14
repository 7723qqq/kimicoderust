use std::collections::BTreeMap;

use serde_json::Value;

pub(crate) use crate::tool::tool_contract::canonical_value;
pub use crate::tool::tool_contract::{
    ToolDefinition, ToolDisclosure, ToolSource, schema_signature, version_signature,
};

#[derive(Debug, Clone)]
struct RegistryEntry {
    tool: ToolDefinition,
    info: ToolDefinition,
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
    revision: u64,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn register(&mut self, tool: ToolDefinition) -> RegistrationStatus {
        self.revision += 1;
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

    pub fn list_references(&self) -> Vec<(String, ToolSource)> {
        let mut references: Vec<(String, ToolSource)> = self
            .active
            .iter()
            .map(|(name, entry)| (name.clone(), entry.tool.source))
            .collect();
        references.sort_by(|a, b| a.0.cmp(&b.0));
        references
    }

    pub fn resolve(&self, name: &str) -> Option<&ToolDefinition> {
        self.active.get(name).map(|entry| &entry.tool)
    }

    pub fn unregister(&mut self, name: &str) {
        self.pending.remove(name);
        if self.active.remove(name).is_some() {
            self.revision += 1;
        }
    }

    /// v2's registration `dispose`: drop the entry only while it is still the
    /// one this registration installed — a newer same-name registration has
    /// already replaced it, and disposing that would evict someone else's tool.
    pub fn dispose(&mut self, name: &str, version: &str) {
        if let Some(entry) = self.active.get(name) {
            if version_signature(&entry.tool) == version {
                self.unregister(name);
                return;
            }
        }
        if let Some(tool) = self.pending.get(name) {
            if version_signature(tool) == version {
                self.pending.remove(name);
                self.revision += 1;
            }
        }
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
    fn the_revision_advances_on_register_and_unregister_only() {
        let mut registry = ToolRegistry::new();
        assert_eq!(registry.revision(), 0);
        registry.register(tool("X", "d", serde_json::json!({})));
        assert_eq!(registry.revision(), 1);
        registry.register(tool("Y", "d", serde_json::json!({})));
        assert_eq!(registry.revision(), 2);
        registry.unregister("Y");
        assert_eq!(registry.revision(), 3);
        registry.unregister("missing");
        assert_eq!(registry.revision(), 3);
    }

    #[test]
    fn references_list_the_name_and_source_in_name_order() {
        let mut registry = ToolRegistry::new();
        registry.register(tool("B", "d", serde_json::json!({})));
        registry.register(tool("A", "d", serde_json::json!({})));
        let references = registry.list_references();
        let names: Vec<&str> = references.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, vec!["A", "B"]);
        assert!(references
            .iter()
            .all(|(_, source)| *source == ToolSource::Mcp));
    }

    #[test]
    fn dispose_removes_only_the_entry_that_registration_installed() {
        let mut registry = ToolRegistry::new();
        let first = tool("X", "first", serde_json::json!({}));
        registry.register(first.clone());
        let first_version = version_signature(&first);
        registry.unregister("X");
        registry.register(tool("X", "second", serde_json::json!({})));
        registry.dispose("X", &first_version);
        assert_eq!(
            registry.list()[0].description, "second",
            "a newer same-name registration survives"
        );

        let pending = tool("Y", "pending", serde_json::json!({}));
        registry.register(pending.clone());
        registry.register(tool("Y", "changed", serde_json::json!({})));
        assert!(registry.has_pending("Y"));
        registry.dispose("Y", &version_signature(&pending));
        assert!(
            !registry.has_pending("Y"),
            "the pending entry that registration installed is dropped"
        );
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
