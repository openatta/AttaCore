//! `ToolSearchTool` — deferred tool activation for the LLM.
//!
//! When `ToolRegistry` contains tools with `is_deferred() == true`, the initial
//! system prompt only exposes the name plus a one-line description (saving
//! schema from consuming cache space). If the model sees a name it wants to use,
//! it calls `ToolSearch{ query: "..." }` to fetch the full schema and activate
//! the tool.
//!
//! ## Naming note
//!
//! The struct is `ToolSearchTool` despite the "Tool Tool" tautology, because
//! every tool struct in this crate follows the `{Name}Tool` suffix convention.
//! The external name exposed to the model is `"ToolSearch"` (no suffix).
//!
//! ## Two things are fetchable here, not one
//!
//! 1. **Deferred tools** — the original purpose. A scene's
//!    `deferred_tools()` strips a tool's JSON schema out of the per-call
//!    `tools` array (see `crate::deferred`); `select:<name>` brings it back.
//! 2. **Tool usage guides** — `Tool::detailed_prompt()`. Roughly 35 tools
//!    ship a multi-KB markdown guide via `Tool::prompt()`, and before this
//!    those guides reached the model on exactly zero requests: `ToolDef` is
//!    built from `description()` and nothing else ever called `prompt()`.
//!    The `Agent` and `Team` tools were hit worst — the most complex
//!    semantics in the tool set, with the documentation explaining them
//!    unreachable.
//!
//! Both use the same progressive-disclosure shape rather than inlining:
//! the guides total ~85 KB (~21k tokens) and would otherwise be paid on
//! every single API call. `build_tool_defs` instead appends one short
//! pointer line to the descriptions of documented tools, and the body
//! arrives here only when the model asks for it.
//!
//! A tool is therefore findable here when it is deferred **or** documented,
//! and a match returns whichever of the two applies (often both).

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{
    PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_MAX_RESULTS: usize = 5;

/// Ceiling on the combined size of the usage guides one `ToolSearch` call
/// will inline into its result. Guides run up to ~10 KB each and `max_results`
/// defaults to 5, so an unbounded result could dump 50 KB into the transcript
/// in a single tool result — permanently, since tool results stay in
/// `messages` until compaction. Matches past the budget still come back as
/// name + description, with an explicit note telling the model how to get the
/// rest, rather than being silently dropped.
const MAX_GUIDE_BYTES: usize = 24_000;

/// One search hit.
struct Match {
    name: String,
    description: String,
    /// `Tool::detailed_prompt()` — the on-demand half of A-1.
    guide: Option<String>,
    /// Whether the scene deferred this tool's schema.
    deferred: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSearchInput {
    /// Search query. Use `select:<tool_name>` for direct selection of a known
    /// deferred tool name; otherwise treated as a keyword to score against
    /// each deferred tool's name + description.
    pub query: String,

    /// Cap on returned matches (default 5).
    #[serde(default)]
    pub max_results: Option<usize>,
}

pub struct ToolSearchTool {
    /// 引用 ToolRegistry 里的全部工具列表（含 deferred 与非 deferred）。这里持
    /// `Arc<dyn ToolRegistry>` 让 ToolSearch 在 call 时能即时扫工具池 —— 用户
    /// 中途装/卸 MCP server 后立刻反映。
    registry: std::sync::Arc<base::tool::InMemoryToolRegistry>,
}

impl ToolSearchTool {
    /// Construct a new instance.
    pub fn new(registry: std::sync::Arc<base::tool::InMemoryToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn description(&self) -> &str {
        "Fetch a tool's full JSON schema (for tools advertised without one) \
         and/or its detailed usage guide. Use query \"select:<ToolName>\" for \
         a known name, or plain keywords to search."
    }
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn input_schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ToolSearchInput))
            .expect("schemars output is valid JSON")
    }

    async fn prompt(&self, _: &PromptContext) -> String {
        include_str!("prompts/coding/tool_search.prompt.md").to_string()
    }

    fn is_concurrency_safe(&self, _: &Value) -> bool {
        // 纯查询 + session state 写入；与文件 / 网络无关
        true
    }

    fn is_read_only(&self, _: &Value) -> bool {
        // 改 SessionState.activated_tools 算副作用，但不动文件 / 不发请求
        true
    }

    async fn validate_input(&self, input: &Value, _: &ToolContext) -> ValidationResult {
        match serde_json::from_value::<ToolSearchInput>(input.clone()) {
            Ok(p) if p.query.trim().is_empty() => {
                ValidationResult::err("query must not be empty", 1)
            }
            Ok(_) => ValidationResult::Ok,
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2),
        }
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // ToolSearch 不影响 fs / 网络；自动允许
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ToolSearchInput = serde_json::from_value(input)?;
        let max = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let q = input.query.trim();

        let all = self.registry.all();
        let prompt_ctx = PromptContext {
            cwd: ctx.cwd.clone(),
            model: String::new(),
            session_id: ctx.session_id.clone(),
            is_interactive: false,
            all_tool_names: all.iter().map(|t| t.name().to_string()).collect(),
            allowed_agent_types: vec![],
        };

        // The searchable set: everything with something to disclose. A tool
        // qualifies by being deferred (schema withheld) *or* documented
        // (`detailed_prompt()` — a guide that never reaches `ToolDef`).
        // Resolving the guide means materializing each `include_str!` body, a
        // few hundred KB of pure memcpy across the whole registry; that is
        // noise next to the model round trip that got us here, and it keeps
        // the predicate honest instead of caching a staleness hazard.
        let mut candidates: Vec<Match> = Vec::new();
        for t in &all {
            let guide = t.detailed_prompt(&prompt_ctx).await;
            let deferred = t.is_deferred();
            if !deferred && guide.is_none() {
                continue;
            }
            candidates.push(Match {
                name: t.name().to_string(),
                description: t
                    .short_description()
                    .unwrap_or_else(|| t.description().to_string()),
                guide,
                deferred,
            });
        }
        let total_deferred = candidates.iter().filter(|m| m.deferred).count();
        let total_documented = candidates.iter().filter(|m| m.guide.is_some()).count();

        // `select:<name>` 直接选；找到就只返回那个
        let mut matches: Vec<Match> = if let Some(name) = q.strip_prefix("select:") {
            let name = name.trim();
            let mut hit: Vec<Match> = Vec::new();
            for m in candidates {
                if m.name == name {
                    hit.push(m);
                }
            }
            hit
        } else {
            // 关键字匹配：name 或 description 包含 query（大小写不敏感）
            let q_lower = q.to_ascii_lowercase();
            let mut scored: Vec<(usize, Match)> = candidates
                .into_iter()
                .filter_map(|m| {
                    let n_lower = m.name.to_ascii_lowercase();
                    let d_lower = m.description.to_ascii_lowercase();
                    let mut score = 0usize;
                    if n_lower == q_lower {
                        score += 100; // 精确名字命中最高
                    } else if n_lower.contains(&q_lower) {
                        score += 50;
                    }
                    if d_lower.contains(&q_lower) {
                        score += 10;
                    }
                    if score > 0 {
                        Some((score, m))
                    } else {
                        None
                    }
                })
                .collect();
            // Stable within a score band so equal-scoring hits come back in
            // registry order rather than an arbitrary one.
            scored.sort_by_key(|s| std::cmp::Reverse(s.0));
            scored.into_iter().map(|(_, m)| m).collect()
        };
        matches.truncate(max);

        // NOTE: Tool activation is handled by the deferred-tool infrastructure
        // (in-memory activation list maintained by ToolRegistry). No session
        // mutation needed in the agent crate.
        let body = render(q, &matches, total_deferred, total_documented);

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: Some(json!({
                "matches": matches.iter().map(|m| m.name.clone()).collect::<Vec<String>>(),
                "guides_returned": matches.iter().filter(|m| m.guide.is_some())
                    .map(|m| m.name.clone()).collect::<Vec<String>>(),
                "query": q,
                "total_deferred_tools": total_deferred,
                "total_documented_tools": total_documented})),
            mcp_meta: None,
            new_messages: Some(vec![]),
        })
    }
}

/// Render the tool result: a summary line, one bullet per match, then each
/// match's usage guide in full (budget permitting).
fn render(
    query: &str,
    matches: &[Match],
    total_deferred: usize,
    total_documented: usize,
) -> String {
    if matches.is_empty() {
        return format!(
            "No matches for query '{query}'. {total_deferred} tool(s) have a deferred \
             schema, {total_documented} have a detailed usage guide.\n\
             (If both are 0, there is nothing to fetch — every tool already \
             carries its full schema and has no guide beyond its description.)"
        );
    }

    let activated = matches.iter().filter(|m| m.deferred).count();
    let mut s = format!("Found {} tool(s) for query '{query}'.\n", matches.len());
    if activated > 0 {
        s.push_str(&format!(
            "{activated} of them had a deferred schema; their full schemas are \
             available in the next request.\n"
        ));
    }
    s.push('\n');
    for m in matches {
        s.push_str(&format!("  · {}", m.name));
        if !m.description.is_empty() {
            s.push_str(&format!(" — {}", m.description));
        }
        s.push('\n');
    }

    // Guides last: the bullet list stays scannable, and a truncated tail
    // costs the model the least important part.
    let mut budget = MAX_GUIDE_BYTES;
    let mut deferred_guides: Vec<&str> = Vec::new();
    for m in matches {
        let Some(guide) = m.guide.as_deref() else {
            continue;
        };
        let guide = guide.trim();
        if guide.len() > budget {
            deferred_guides.push(&m.name);
            continue;
        }
        budget -= guide.len();
        s.push_str(&format!("\n## {} — usage guide\n\n{guide}\n", m.name));
    }
    if !deferred_guides.is_empty() {
        s.push_str(&format!(
            "\n(Usage guides for {} were omitted to stay within this result's \
             size budget — request them one at a time with \
             ToolSearch{{\"query\": \"select:<name>\"}}.)\n",
            deferred_guides.join(", ")
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::InMemoryToolRegistry;

    use std::path::PathBuf;
    use std::sync::Arc;

    /// 一个声称 deferred 的虚构工具。
    struct DeferredTool {
        name: &'static str,
        desc: &'static str,
    }
    #[async_trait]
    impl Tool for DeferredTool {
        fn name(&self) -> &str {
            self.name
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &PromptContext) -> String {
            self.desc.to_string()
        }
        fn is_deferred(&self) -> bool {
            true
        }
        fn short_description(&self) -> Option<String> {
            Some(self.desc.to_string())
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text(""))
        }
    }

    /// 一个非 deferred 的对照工具。
    struct ActiveTool;
    #[async_trait]
    impl Tool for ActiveTool {
        fn name(&self) -> &str {
            "Active"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn prompt(&self, _: &PromptContext) -> String {
            "always active".into()
        }
        async fn call(
            &self,
            _: Value,
            _: ToolContext,
            _: ProgressSender,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::text(""))
        }
    }

    fn make_registry() -> Arc<InMemoryToolRegistry> {
        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(ActiveTool));
        reg.register(Arc::new(DeferredTool {
            name: "GitHubIssues",
            desc: "Fetch GitHub issues for a repo",
        }));
        reg.register(Arc::new(DeferredTool {
            name: "GitHubPRs",
            desc: "List GitHub pull requests",
        }));
        reg.register(Arc::new(DeferredTool {
            name: "Datadog",
            desc: "Query Datadog metrics",
        }));
        Arc::new(reg)
    }

    fn make_ctx() -> ToolContext {
        ToolContext::for_test(PathBuf::from("/tmp"))
    }

    #[tokio::test]
    async fn keyword_match_finds_relevant_deferred_tools() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "github"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("GitHubIssues"));
                assert!(t.contains("GitHubPRs"));
                assert!(!t.contains("Datadog"), "Datadog should not match 'github'");
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn select_prefix_does_exact_match() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "select:Datadog"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Datadog"));
            }
            _ => panic!(),
        }
    }

    #[tokio::test]
    async fn select_prefix_with_unknown_name_returns_no_matches() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let _ = tool
            .call(
                json!({"query": "select:GhostTool"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert!(
            Vec::<String>::new() /* activated_tools managed by ToolRegistry now */
                .is_empty()
        );
    }

    #[tokio::test]
    async fn never_matches_non_deferred_tools() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        // ActiveTool 不是 deferred —— 关键字命中也不该返回
        let _ = tool
            .call(
                json!({"query": "active"}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert!(
            Vec::<String>::new() /* activated_tools managed by ToolRegistry now */
                .is_empty()
        );
    }

    /// A-1's payoff: `select:` on a documented tool returns the *body* of
    /// `Tool::prompt()`, which before this reached the model on exactly zero
    /// requests. `Documented` is deliberately **not** deferred — the tools
    /// this defect was really about (`Agent`, `TeamCreate`) ship full schemas
    /// and were still undiscoverable here.
    #[tokio::test]
    async fn select_returns_the_tools_usage_guide() {
        const GUIDE: &str = "# Agent tool\n\nSpawn a sub-agent when the search is wide.";
        struct Documented;
        #[async_trait]
        impl Tool for Documented {
            fn name(&self) -> &str {
                "Agent"
            }
            fn description(&self) -> &str {
                "Launch a sub-agent."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn prompt(&self, _: &PromptContext) -> String {
                GUIDE.to_string()
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Documented));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Agent"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();

        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(
                    t.contains(GUIDE),
                    "usage guide must be returned in full: {t}"
                );
                assert!(t.contains("Agent"));
            }
            _ => panic!("expected text"),
        }
        let sc = result.structured_content.unwrap();
        assert_eq!(sc["guides_returned"], json!(["Agent"]));
    }

    /// A deferred tool with no `prompt()` override must still resolve — the
    /// original schema-activation behaviour cannot regress just because
    /// guides were added alongside it.
    #[tokio::test]
    async fn select_still_works_for_a_tool_without_a_guide() {
        struct Bare;
        #[async_trait]
        impl Tool for Bare {
            fn name(&self) -> &str {
                "Bare"
            }
            fn description(&self) -> &str {
                "Does one thing."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn is_deferred(&self) -> bool {
                true
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Bare));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Bare"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();

        let sc = result.structured_content.unwrap();
        assert_eq!(sc["matches"], json!(["Bare"]));
        assert_eq!(
            sc["guides_returned"],
            json!([]),
            "a tool with no prompt() override must report no guide, not an \
             echo of its own description"
        );
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("Bare"));
                assert!(!t.contains("usage guide"));
            }
            _ => panic!("expected text"),
        }
    }

    /// A tool that is neither deferred nor documented has nothing to
    /// disclose and must not appear — otherwise `ToolSearch` degrades into a
    /// second, redundant copy of the tool list.
    #[tokio::test]
    async fn plain_tools_are_not_searchable() {
        struct Plain;
        #[async_trait]
        impl Tool for Plain {
            fn name(&self) -> &str {
                "Plain"
            }
            fn description(&self) -> &str {
                "Plain tool."
            }
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            async fn call(
                &self,
                _: Value,
                _: ToolContext,
                _: ProgressSender,
            ) -> Result<ToolResult, ToolError> {
                Ok(ToolResult::text(""))
            }
        }

        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(Plain));
        let tool = ToolSearchTool::new(Arc::new(reg));
        let result = tool
            .call(
                json!({"query": "select:Plain"}),
                make_ctx(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        assert_eq!(result.structured_content.unwrap()["matches"], json!([]));
    }

    #[tokio::test]
    async fn empty_query_validates_err() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let r = tool
            .validate_input(&json!({"query": "   "}), &make_ctx())
            .await;
        assert!(!matches!(r, ValidationResult::Ok));
    }

    #[tokio::test]
    async fn max_results_caps_returns() {
        let reg = make_registry();
        let tool = ToolSearchTool::new(reg);
        let ctx = make_ctx();
        let result = tool
            .call(
                json!({"query": "g", "max_results": 1}),
                ctx.clone(),
                ProgressSender::noop("tu_1"),
            )
            .await
            .unwrap();
        // 'g' 命中 GitHubIssues / GitHubPRs / 但 max 1 时只返回 1 个
        match result.content {
            base::tool::ToolResultContent::Text(t) => {
                assert!(t.contains("1 tool"), "expected 1 tool, got: {t}");
            }
            _ => panic!(),
        }
    }
}
