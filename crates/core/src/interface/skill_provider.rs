//! `SkillProvider` — where a session's skills come from.
//!
//! Skills arrive from three places: `SKILL.md` files under the three
//! directory tiers, the built-ins compiled into the binary, and the tools of
//! every connected MCP server turned into invocable skills. Each of those had
//! its own entry point on `SkillManager` — `load_dir_subdirs`,
//! `register_bundled`, `register_mcp_skills` — so a deployment that keeps its
//! skills anywhere else (a database, a company-wide bundle server, a git
//! remote) had nowhere to say so short of a fourth method on the manager.
//!
//! # A source owns its bodies too
//!
//! Listing a skill and expanding it are two different reads, and only the
//! first one is satisfied by metadata. The built-in sources that hold their
//! content in memory said so through a sentinel `path` — `(bundled:name)`,
//! `(mcp:server:tool)` — that the manager pattern-matched on. A provider says
//! it directly instead, via [`SkillProvider::body`], so a source whose skills
//! are not files is a complete source rather than one that lists skills
//! nobody can run.

use crate::frozen::skill::SkillEntry;

/// What a provider does about a name that is already loaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillPrecedence {
    /// Replace what is there.
    Override,
    /// Leave what is there and register only the names nobody claimed.
    Fallback,
}

/// A place skills come from.
pub trait SkillProvider: Send + Sync {
    /// Stable identifier, for logs and for a host that wants to find its own
    /// source again.
    fn id(&self) -> &str;

    /// What this source does about a name another source already registered.
    ///
    /// The engine's own answer is that the disk wins: a project that writes
    /// its own `review` skill means to override the built-in one.
    fn precedence(&self) -> SkillPrecedence {
        SkillPrecedence::Fallback
    }

    /// Everything this source has, as of now.
    fn skills(&self) -> Vec<SkillEntry>;

    /// The full text of one of this source's skills, when the source holds it
    /// rather than a file does. `None` means "read the entry's `path`", which
    /// is the right answer for any source backed by the filesystem.
    fn body(&self, _name: &str) -> Option<String> {
        None
    }
}

/// A fixed list of skills held in memory.
///
/// The shape an embedding program reaches for: it already has the skills —
/// fetched from its own service, generated from its own configuration, or
/// written inline in the host binary — and wants them in the catalog without
/// first writing them to a directory for the engine to scan back.
pub struct StaticSkills {
    id: String,
    entries: Vec<SkillEntry>,
    bodies: std::collections::HashMap<String, String>,
    precedence: SkillPrecedence,
}

impl StaticSkills {
    pub fn new(id: impl Into<String>, entries: Vec<SkillEntry>) -> Self {
        Self {
            id: id.into(),
            entries,
            bodies: std::collections::HashMap::new(),
            precedence: SkillPrecedence::Fallback,
        }
    }

    /// Supply the text for one skill, for entries whose `path` points at
    /// nothing readable.
    pub fn with_body(mut self, name: impl Into<String>, body: impl Into<String>) -> Self {
        self.bodies.insert(name.into(), body.into());
        self
    }

    pub fn overriding(mut self) -> Self {
        self.precedence = SkillPrecedence::Override;
        self
    }
}

impl SkillProvider for StaticSkills {
    fn id(&self) -> &str {
        &self.id
    }

    fn precedence(&self) -> SkillPrecedence {
        self.precedence
    }

    fn skills(&self) -> Vec<SkillEntry> {
        self.entries.clone()
    }

    fn body(&self, name: &str) -> Option<String> {
        self.bodies.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> SkillEntry {
        SkillEntry {
            name: name.to_string(),
            description: format!("{name} description"),
            ..Default::default()
        }
    }

    #[test]
    fn a_static_source_carries_its_own_bodies() {
        let source = StaticSkills::new("host", vec![entry("deploy")])
            .with_body("deploy", "run the deploy checklist");
        assert_eq!(source.id(), "host");
        assert_eq!(source.skills().len(), 1);
        assert_eq!(
            source.body("deploy").as_deref(),
            Some("run the deploy checklist")
        );
        assert!(source.body("nothing").is_none());
    }

    #[test]
    fn a_source_defers_to_what_is_already_loaded_unless_it_says_otherwise() {
        assert_eq!(
            StaticSkills::new("host", vec![]).precedence(),
            SkillPrecedence::Fallback
        );
        assert_eq!(
            StaticSkills::new("host", vec![]).overriding().precedence(),
            SkillPrecedence::Override
        );
    }
}
