//! Task routing — classifies user requests into coding task kinds.
//!
//! # Classification strategy
//!
//! Two-layer heuristic:
//! 1. Strong signal keywords (explicit task names in user text)
//! 2. Pattern matching (query structure, error presence, diff context)
//!
//! Falls back to `Modify` — the most general coding task.

/// Coding task kind — what the user is asking the agent to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodingTaskKind {
    /// Explain code, architecture, or behavior — read-only.
    Explain,
    /// Search the codebase for definitions, usages, patterns — read-only.
    Search,
    /// Generate new code from scratch.
    Generate,
    /// Modify existing code (bug fix, feature addition, refactor).
    Modify,
    /// Debug a failure — root-cause analysis followed by fix.
    Debug,
    /// Review code changes — audit, risk assessment, no edits.
    Review,
    /// Refactor existing code — restructure without changing behavior.
    Refactor,
    /// Write documentation, comments, README.
    Document,
    /// Plan architecture or design — no code changes.
    Plan,
}

impl CodingTaskKind {
    /// String representation for config keys.
    pub fn as_str(&self) -> &'static str {
        match self {
            CodingTaskKind::Explain => "explain",
            CodingTaskKind::Search => "search",
            CodingTaskKind::Generate => "generate",
            CodingTaskKind::Modify => "modify",
            CodingTaskKind::Debug => "debug",
            CodingTaskKind::Review => "review",
            CodingTaskKind::Refactor => "refactor",
            CodingTaskKind::Document => "document",
            CodingTaskKind::Plan => "plan",
        }
    }

    /// All task kinds.
    pub fn all() -> &'static [CodingTaskKind] {
        &[
            CodingTaskKind::Explain,
            CodingTaskKind::Search,
            CodingTaskKind::Generate,
            CodingTaskKind::Modify,
            CodingTaskKind::Debug,
            CodingTaskKind::Review,
            CodingTaskKind::Refactor,
            CodingTaskKind::Document,
            CodingTaskKind::Plan,
        ]
    }
}

/// Trait for task classification — extensibility point for ML-based classifiers.
pub trait TaskClassifier: Send + Sync {
    /// Classify a user message. Returns None if uncertain.
    fn classify(&self, prompt: &str) -> Option<CodingTaskKind>;
}

/// Rule-based task router using heuristics.
///
/// Fast (<1ms), no model calls. Recalls > 90% for explicit task requests.
#[derive(Debug, Clone, Default)]
pub struct RuleBasedTaskRouter;

impl TaskClassifier for RuleBasedTaskRouter {
    fn classify(&self, prompt: &str) -> Option<CodingTaskKind> {
        classify_task(prompt)
    }
}

impl RuleBasedTaskRouter {
    pub fn new() -> Self {
        Self
    }

    /// Classify a user message. Always returns a kind (defaults to Modify).
    pub fn route(&self, prompt: &str) -> CodingTaskKind {
        classify_task(prompt).unwrap_or(CodingTaskKind::Modify)
    }
}

/// Heuristic task classifier.
fn classify_task(prompt: &str) -> Option<CodingTaskKind> {
    let p = prompt.trim();
    if p.is_empty() {
        return None;
    }
    let pl = p.to_lowercase();

    // ── Layer 1: Strong signal keywords ──

    // Debug — explicit error/failure language
    let debug_markers = [
        "debug",
        "fix this bug",
        "fix the bug",
        "why is this failing",
        "why is it failing",
        "why does this fail",
        "test is failing",
        "tests are failing",
        "build is broken",
        "something is broken",
        "not working",
        "doesn't work",
        "does not work",
        "crash",
        "segfault",
        "null pointer",
        "panic",
        "traceback",
        "stack trace",
        "exception",
    ];
    if debug_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Debug);
    }

    // Review — explicit review request
    let review_markers = [
        "review",
        "code review",
        "audit this",
        "audit the",
        "check this code",
        "is this safe",
        "security review",
        "look over",
        "inspect this",
        "examine this",
    ];
    if review_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Review);
    }

    // Refactor — explicit refactoring request
    let refactor_markers = [
        "refactor",
        "restructure",
        "reorganize",
        "clean up the code",
        "clean up this code",
        "cleanup",
        "split into",
        "extract method",
        "extract function",
        "extract class",
        "extract module",
        "break apart",
    ];
    if refactor_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Refactor);
    }

    // Explain — explicit understanding request
    let explain_markers = [
        "explain",
        "what does this",
        "what does that",
        "what is this",
        "what is that",
        "how does this work",
        "how does that work",
        "walk me through",
        "describe",
        "tell me about",
        "what's the purpose",
        "what is the purpose",
    ];
    if explain_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Explain);
    }

    // Search — explicit find/locate request
    let search_markers = [
        "find",
        "search for",
        "locate",
        "where is",
        "where are",
        "grep for",
        "look for",
        "show me where",
        "which file",
        "what files",
    ];
    if search_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Search);
    }

    // Document — explicit documentation request
    let doc_markers = [
        "document",
        "write doc",
        "add doc",
        "add comment",
        "write comment",
        "write a readme",
        "documentation",
        "docstring",
        "javadoc",
        "jsdoc",
    ];
    if doc_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Document);
    }

    // Plan — explicit planning/design request
    let plan_markers = [
        "plan",
        "design",
        "architecture",
        "how should i build",
        "how should we build",
        "how would you build",
        "how to implement",
        "approach for",
        "strategy for",
        "proposal",
    ];
    if plan_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Plan);
    }

    // Generate — explicit creation request (new file, new project)
    let gen_markers = [
        "create",
        "generate",
        "write a",
        "build a",
        "scaffold",
        "new project",
        "new file",
        "from scratch",
    ];
    if gen_markers.iter().any(|m| pl.contains(m)) {
        return Some(CodingTaskKind::Generate);
    }

    // ── Layer 2: Pattern matching ──

    // Error/failure patterns → Debug (check BEFORE question patterns so
    // "why did this fail" and "为什么这个测试失败了" classify as Debug)
    if pl.contains("error")
        || pl.contains("failed")
        || pl.contains("failure")
        || pl.contains("fail")
        || pl.contains("wrong")
        || pl.contains("bug")
        || pl.contains("broke")
        // Chinese error keywords
        || pl.contains("错误")
        || pl.contains("失败")
        || pl.contains("异常")
        || pl.contains("报错")
        || pl.contains("坏了")
        || pl.contains("不行")
    {
        return Some(CodingTaskKind::Debug);
    }

    // Diff/PR patterns → Review
    if pl.contains("diff")
        || pl.contains("pull request")
        || pl.contains("pr")
        || pl.contains("change")
    {
        return Some(CodingTaskKind::Review);
    }

    // Question → Explain
    if pl.starts_with("what") || pl.starts_with("how") || pl.starts_with("why") || pl.ends_with("?")
    {
        return Some(CodingTaskKind::Explain);
    }

    // Default: can't classify confidently
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_debug_from_error() {
        assert_eq!(
            classify_task("cargo test is failing with a panic"),
            Some(CodingTaskKind::Debug)
        );
        assert_eq!(
            classify_task("为什么这个测试失败了"),
            Some(CodingTaskKind::Debug)
        );
    }

    #[test]
    fn classify_explain() {
        assert_eq!(
            classify_task("explain this function"),
            Some(CodingTaskKind::Explain)
        );
        assert_eq!(
            classify_task("what does this code do"),
            Some(CodingTaskKind::Explain)
        );
    }

    #[test]
    fn classify_review() {
        assert_eq!(
            classify_task("review this diff"),
            Some(CodingTaskKind::Review)
        );
        assert_eq!(
            classify_task("code review please"),
            Some(CodingTaskKind::Review)
        );
    }

    #[test]
    fn classify_refactor() {
        assert_eq!(
            classify_task("refactor this module"),
            Some(CodingTaskKind::Refactor)
        );
    }

    #[test]
    fn classify_search() {
        assert_eq!(
            classify_task("find all uses of parse_config"),
            Some(CodingTaskKind::Search)
        );
        assert_eq!(
            classify_task("where is the login handler"),
            Some(CodingTaskKind::Search)
        );
    }

    #[test]
    fn classify_document() {
        assert_eq!(
            classify_task("add comments to this function"),
            Some(CodingTaskKind::Document)
        );
    }

    #[test]
    fn classify_plan() {
        assert_eq!(
            classify_task("design the architecture for user auth"),
            Some(CodingTaskKind::Plan)
        );
    }

    #[test]
    fn classify_generate() {
        assert_eq!(
            classify_task("write a tcp echo server"),
            Some(CodingTaskKind::Generate)
        );
        assert_eq!(
            classify_task("create a new react component"),
            Some(CodingTaskKind::Generate)
        );
    }

    #[test]
    fn router_defaults_to_modify() {
        let router = RuleBasedTaskRouter::new();
        assert_eq!(router.route(""), CodingTaskKind::Modify);
        assert_eq!(
            router.route("update the config parser"),
            CodingTaskKind::Modify
        );
    }
}
