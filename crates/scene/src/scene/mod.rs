//! Built-in scene implementations.
//!
//! Scenes are code-level AgentScene implementations, registered at compile time.
//! AttaCode provides `coding` (Claude Code parity) and `demo` (framework showcase).

pub mod chat;
pub mod coding;
pub mod demo;

use base::interface::scene::AgentScene;
use std::collections::HashMap;
use std::sync::Arc;

/// Scene metadata for discovery.
#[derive(Debug, Clone)]
pub struct SceneInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Registry of available scenes. Populated at process startup.
#[derive(Default)]
pub struct SceneRegistry {
    scenes: HashMap<String, Arc<dyn AgentScene>>,
}

impl SceneRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, scene: Arc<dyn AgentScene>) {
        self.scenes.insert(scene.id().to_string(), scene);
    }
    pub fn resolve(&self, id: &str) -> Option<Arc<dyn AgentScene>> {
        self.scenes.get(id).cloned()
    }
    pub fn ids(&self) -> Vec<String> {
        self.scenes.keys().cloned().collect()
    }
    pub fn list_all(&self) -> Vec<SceneInfo> {
        self.scenes
            .iter()
            .map(|(id, s)| SceneInfo {
                id: id.clone(),
                name: s.name().to_string(),
                description: s.description().to_string(),
            })
            .collect()
    }
    pub fn register_builtin(&mut self) {
        self.register(Arc::new(chat::ChatScene));
        self.register(Arc::new(coding::CodingScene));
        self.register(Arc::new(demo::DemoScene));
    }
}
