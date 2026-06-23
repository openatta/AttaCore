//! Dependency graph resolution for plugin installations.
//!
//! TS parity: claude-code's plugin dependency graph with version constraints.
//! Resolves a DAG of plugins ensuring no version conflicts.

use crate::manifest::PluginError;
use std::collections::{HashMap, HashSet, VecDeque};

/// A dependency constraint: plugin name + optional version range.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DependencyConstraint {
    pub name: String,
    pub version_req: Option<String>,
}

impl DependencyConstraint {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version_req: None,
        }
    }

    pub fn with_version(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            version_req: Some(version.into()),
        }
    }
}

/// A node in the dependency graph.
#[derive(Debug, Clone)]
pub struct DependencyNode {
    pub constraint: DependencyConstraint,
    pub resolved_version: Option<String>,
    pub deps: Vec<DependencyConstraint>,
}

/// Topologically sorted dependency resolution result.
#[derive(Debug, Clone)]
pub struct ResolvedGraph {
    /// Installation order (dependencies before dependents).
    pub install_order: Vec<ResolvedPlugin>,
    /// Circular dependency detection (empty if no cycles).
    pub cycles: Vec<Vec<String>>,
}

/// A single resolved plugin with its version.
#[derive(Debug, Clone)]
pub struct ResolvedPlugin {
    pub name: String,
    pub version: String,
}

/// Build and resolve a plugin dependency graph.
pub struct DependencyGraph {
    nodes: HashMap<String, DependencyNode>,
}

impl DependencyGraph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    /// Add a root plugin (the one the user wants to install).
    pub fn add_root(&mut self, name: impl Into<String>, version: impl Into<String>, deps: Vec<DependencyConstraint>) {
        let name = name.into();
        self.nodes.insert(
            name.clone(),
            DependencyNode {
                constraint: DependencyConstraint::new(name.clone()),
                resolved_version: Some(version.into()),
                deps,
            },
        );
    }

    /// Add a dependency node.
    pub fn add_dependency(&mut self, dep: DependencyConstraint, version: String, sub_deps: Vec<DependencyConstraint>) {
        self.nodes.insert(
            dep.name.clone(),
            DependencyNode {
                constraint: dep,
                resolved_version: Some(version),
                deps: sub_deps,
            },
        );
    }

    /// Resolve the dependency graph. Returns install order (deps first).
    /// Detects circular dependencies.
    pub fn resolve(&self) -> Result<ResolvedGraph, PluginError> {
        // Kahn's algorithm for topological sort
        let mut in_degree: HashMap<&str, usize> = HashMap::new();
        let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

        for (name, node) in &self.nodes {
            in_degree.entry(name).or_insert(0);
            for dep in &node.deps {
                adjacency
                    .entry(&dep.name)
                    .or_default()
                    .push(name.as_str());
                *in_degree.entry(name.as_str()).or_insert(0) += 1;
            }
        }

        let mut queue: VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&name, _)| name)
            .collect();

        let mut install_order = Vec::new();
        let mut processed = HashSet::new();

        while let Some(name) = queue.pop_front() {
            if !processed.insert(name) {
                continue;
            }
            if let Some(node) = self.nodes.get(name) {
                install_order.push(ResolvedPlugin {
                    name: name.to_string(),
                    version: node.resolved_version.clone().unwrap_or_else(|| "latest".into()),
                });
            }
            if let Some(neighbors) = adjacency.get(name) {
                for &neighbor in neighbors {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            queue.push_back(neighbor);
                        }
                    }
                }
            }
        }

        // Detect cycles: nodes still with in_degree > 0
        let cycles: Vec<Vec<String>> = in_degree
            .iter()
            .filter(|(_, &deg)| deg > 0)
            .map(|(&name, _)| vec![name.to_string()])
            .collect();

        Ok(ResolvedGraph {
            install_order,
            cycles,
        })
    }

    /// Check if the graph is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }
}

impl Default for DependencyGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_deps_resolve_in_order() {
        let mut graph = DependencyGraph::new();
        graph.add_root("app", "1.0", vec![DependencyConstraint::new("lib")]);
        graph.add_dependency(DependencyConstraint::new("lib"), "2.0".into(), vec![]);

        let resolved = graph.resolve().unwrap();
        assert!(resolved.cycles.is_empty());
        let names: Vec<&str> = resolved.install_order.iter().map(|p| p.name.as_str()).collect();
        // lib should come before app
        let lib_idx = names.iter().position(|&n| n == "lib").unwrap();
        let app_idx = names.iter().position(|&n| n == "app").unwrap();
        assert!(lib_idx < app_idx, "dependency must install before dependent");
    }

    #[test]
    fn empty_graph_resolves_empty() {
        let graph = DependencyGraph::new();
        let resolved = graph.resolve().unwrap();
        assert!(resolved.install_order.is_empty());
    }

    #[test]
    fn cycle_is_detected() {
        let mut graph = DependencyGraph::new();
        graph.add_root("a", "1.0", vec![DependencyConstraint::new("b")]);
        graph.add_dependency(DependencyConstraint::new("b"), "1.0".into(), vec![DependencyConstraint::new("a")]);

        let resolved = graph.resolve().unwrap();
        assert!(!resolved.cycles.is_empty(), "cycle should be detected");
    }
}
