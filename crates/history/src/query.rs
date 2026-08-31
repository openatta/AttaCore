//! The vocabulary for asking the history for sessions rather than for one
//! session.
//!
//! `/resume` needs "the last ten here", "the ones mentioning postgres", "the
//! ones anywhere under this repo". Those are three shapes of the same
//! question, and [`SessionQuery`] is that question as a value so a backend can
//! answer it any way it can — a directory walk, an index, a `SELECT`.

use std::path::PathBuf;

/// Which sessions are in range.
///
/// A backend that keeps sessions in per-project directories can narrow before
/// it reads anything; one that keeps a flat set has nothing to narrow and
/// answers every scope from everything it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionScope {
    /// The project this store is bound to.
    CurrentProject,
    /// Every project the store can reach.
    AllProjects,
    /// Projects whose working directory sits under this path.
    Under(PathBuf),
}

/// What the caller is looking for.
///
/// `text` is matched trimmed and case-insensitively; empty means "no filter",
/// which is how "the most recent ones" is expressed. `limit` is a ceiling, not
/// a target — fewer is a normal answer, more is a contract violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionQuery {
    pub text: String,
    pub scope: SessionScope,
    pub limit: usize,
}

impl SessionQuery {
    /// The most recent sessions in the store's own project.
    pub fn recent(limit: usize) -> Self {
        Self {
            text: String::new(),
            scope: SessionScope::CurrentProject,
            limit,
        }
    }

    /// Sessions in the store's own project matching `text`.
    pub fn matching(text: impl Into<String>, limit: usize) -> Self {
        Self {
            text: text.into(),
            scope: SessionScope::CurrentProject,
            limit,
        }
    }

    pub fn within(mut self, scope: SessionScope) -> Self {
        self.scope = scope;
        self
    }

    /// The text to match on, or `None` when everything matches.
    pub fn needle(&self) -> Option<&str> {
        let trimmed = self.text.trim();
        (!trimmed.is_empty()).then_some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_only_text_is_no_filter() {
        assert_eq!(SessionQuery::matching("  \t ", 10).needle(), None);
        assert_eq!(SessionQuery::recent(10).needle(), None);
        assert_eq!(SessionQuery::matching(" needle ", 10).needle(), Some("needle"));
    }

    #[test]
    fn scope_defaults_to_the_stores_own_project() {
        assert_eq!(SessionQuery::recent(1).scope, SessionScope::CurrentProject);
        assert_eq!(
            SessionQuery::matching("x", 1)
                .within(SessionScope::AllProjects)
                .scope,
            SessionScope::AllProjects
        );
    }
}
