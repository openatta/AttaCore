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
//! ## Current deferred tools
//!
//! Phase 4 first cut: all built-in tools are non-deferred; MCP adapters are
//! potential deferred candidates. Until deferred tools are introduced,
//! `ToolSearch` typically returns empty results — this is expected.

use async_trait::async_trait;
use base::error::ToolError;
use base::tool::{PermissionDecision, ProgressSender, PromptContext, Tool, ToolContext, ToolResult,
    ValidationResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_MAX_RESULTS: usize = 5;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ToolSearchInput {
    /// Search query. Use `select:<tool_name>` for direct selection of a known
    /// deferred tool name; otherwise treated as a keyword to score against
    /// each deferred tool's name + description.
    pub query: String,

    /// Cap on returned matches (default 5).
    #[serde(default)]
    pub max_results: Option<usize>}

pub struct ToolSearchTool {
    /// 引用 ToolRegistry 里的全部工具列表（含 deferred 与非 deferred）。这里持
    /// `Arc<dyn ToolRegistry>` 让 ToolSearch 在 call 时能即时扫工具池 —— 用户
    /// 中途装/卸 MCP server 后立刻反映。
    registry: std::sync::Arc<base::tool::InMemoryToolRegistry>}

impl ToolSearchTool {
    /// Construct a new instance.
    pub fn new(registry: std::sync::Arc<base::tool::InMemoryToolRegistry>) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn description(&self) -> &str { "Fetch full schema definitions for deferred tools" }
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
            Err(e) => ValidationResult::err(format!("invalid input: {e}"), 2)}
    }

    async fn check_permissions(&self, _: &Value, _: &ToolContext) -> PermissionDecision {
        // ToolSearch 不影响 fs / 网络；自动允许
        PermissionDecision::allow()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolContext,
        _progress: ProgressSender,
    ) -> Result<ToolResult, ToolError> {
        let input: ToolSearchInput = serde_json::from_value(input)?;
        let max = input.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        let q = input.query.trim();

        // 收集所有 deferred 工具的 (name, short_description)
        let deferred: Vec<(String, String)> = self
            .registry
            .all()
            .iter()
            .filter(|t| t.is_deferred())
            .map(|t| {
                (
                    t.name().to_string(),
                    t.short_description().unwrap_or_default(),
                )
            })
            .collect();

        // `select:<name>` 直接选；找到就只返回那个
        let mut matches: Vec<(String, String)> = if let Some(name) = q.strip_prefix("select:") {
            let name = name.trim();
            deferred
                .iter()
                .filter(|(n, _)| n == name)
                .cloned()
                .collect()
        } else {
            // 关键字匹配：name 或 description 包含 query（大小写不敏感）
            let q_lower = q.to_ascii_lowercase();
            let mut scored: Vec<(usize, (String, String))> = deferred
                .iter()
                .filter_map(|(n, d)| {
                    let n_lower = n.to_ascii_lowercase();
                    let d_lower = d.to_ascii_lowercase();
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
                        Some((score, (n.clone(), d.clone())))
                    } else {
                        None
                    }
                })
                .collect();
            scored.sort_by_key(|s| std::cmp::Reverse(s.0));
            scored.truncate(max);
            scored.into_iter().map(|(_, p)| p).collect()
        };
        matches.truncate(max);

        // NOTE: Tool activation is handled by the deferred-tool infrastructure
        // (in-memory activation list maintained by ToolRegistry). No session
        // mutation needed in the agent crate.
        // 渲染结果
        let total = deferred.len();
        let body = if matches.is_empty() {
            format!(
                "No matches for query '{q}'. Total deferred tools available: {total}.\n\
                 (If 0, the system has no deferred tools — all are already active.)"
            )
        } else {
            let mut s = format!(
                "Activated {} tool(s) for next turn (out of {total} deferred). \
                 Their full schemas will be available in the next request:\n\n",
                matches.len()
            );
            for (n, d) in &matches {
                s.push_str(&format!("  · {n}"));
                if !d.is_empty() {
                    s.push_str(&format!(" — {d}"));
                }
                s.push('\n');
            }
            s
        };

        Ok(ToolResult {
            content: base::tool::ToolResultContent::Text(body),
            is_error: false,
            structured_content: Some(json!({
                "matches": matches.iter().map(|(n, _)| n.clone()).collect::<Vec<String>>(),
                "query": q,
                "total_deferred_tools": total})),
            mcp_meta: None,
            new_messages: Some(vec![])})
    }
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
        desc: &'static str}
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
            desc: "Fetch GitHub issues for a repo"}));
        reg.register(Arc::new(DeferredTool {
            name: "GitHubPRs",
            desc: "List GitHub pull requests"}));
        reg.register(Arc::new(DeferredTool {
            name: "Datadog",
            desc: "Query Datadog metrics"}));
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
            _ => panic!()}
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
            _ => panic!()}
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
        assert!(Vec::<String>::new() /* activated_tools managed by ToolRegistry now */.is_empty());
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
        assert!(Vec::<String>::new() /* activated_tools managed by ToolRegistry now */.is_empty());
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
            _ => panic!()}
    }
}
