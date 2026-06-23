//! Agent registry — stores plugin-defined agent types for sub-agent dispatch.
//!
//! Plugin manifests can declare custom agent types via `agents` in plugin.toml.
//! Each agent type has a name, description, system prompt path, allowed tools,
//! and optional model override. The registry makes these discoverable at runtime
//! so `AgentTool` can create sub-agents with the plugin's configuration.

use crate::manifest::AgentDef;
use std::collections::HashMap;

/// Runtime registry for plugin-defined agent types.
///
/// AgentTool's `subagent_type` parameter may refer to any agent in this registry.
/// Built-in agent types (explore, plan, general-purpose, worker) are hard-coded
/// in `agent_tool.rs` and are not stored here.
#[derive(Debug, Clone)]
pub struct AgentRegistry {
    agents: HashMap<String, AgentDef>,
}

impl AgentRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// Register a plugin-defined agent type. Replaces any existing entry with
    /// the same name.
    pub fn register(&mut self, def: AgentDef) {
        self.agents.insert(def.name.clone(), def);
    }

    /// Look up an agent type by name.
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.get(name)
    }

    /// Iterate all registered agent types.
    pub fn list(&self) -> Vec<&AgentDef> {
        self.agents.values().collect()
    }

    /// Number of registered agent types.
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_def(name: &str) -> AgentDef {
        AgentDef {
            name: name.into(),
            description: format!("Agent type: {name}"),
            system_prompt_path: PathBuf::from("prompts/agent.md"),
            allowed_tools: vec!["Bash".into(), "Read".into()],
            model: Some("sonnet".into()),
        }
    }

    #[test]
    fn empty_registry() {
        let r = AgentRegistry::new();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
        assert!(r.get("nonexistent").is_none());
    }

    #[test]
    fn register_and_retrieve() {
        let mut r = AgentRegistry::new();
        r.register(sample_def("code-reviewer"));
        assert_eq!(r.len(), 1);
        let def = r.get("code-reviewer").expect("agent should exist");
        assert_eq!(def.description, "Agent type: code-reviewer");
    }

    #[test]
    fn register_overwrites_duplicate() {
        let mut r = AgentRegistry::new();
        r.register(sample_def("dupe"));
        let mut def2 = sample_def("dupe");
        def2.description = "overwritten".into();
        r.register(def2);
        assert_eq!(r.len(), 1);
        assert_eq!(r.get("dupe").unwrap().description, "overwritten");
    }

    #[test]
    fn list_returns_all() {
        let mut r = AgentRegistry::new();
        r.register(sample_def("a"));
        r.register(sample_def("b"));
        assert_eq!(r.list().len(), 2);
    }
}
