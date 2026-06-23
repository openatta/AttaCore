//! Concurrent tool dispatch — ported from attacode-engine/src/engine/turn/dispatch.rs

use crate::agent::EventSender;
use base::interface::event::AgentEvent;
use base::tool::{ProgressSender, ToolContext, ToolResult};
use futures::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// Parameters for [`dispatch_tool_calls`].
///
/// Bundles all arguments into a named struct to avoid long parameter lists
/// and make call sites more readable.
pub struct DispatchParams<'a> {
    pub tools: &'a base::tool::InMemoryToolRegistry,
    /// (id, name, input) tuples.
    pub tool_uses: &'a [(String, String, serde_json::Value)],
    pub cwd: &'a std::path::Path,
    pub session_id: &'a str,
    pub turn_no: u32,
    pub turn_id: &'a str,
    pub max_parallelism: usize,
    pub events: &'a EventSender,
    pub cancel: &'a CancellationToken,
}

pub async fn dispatch_tool_calls(
    params: DispatchParams<'_>,
) -> (Vec<(String, String, ToolResult)>, bool) {
    let mut results: Vec<Option<(String, String, ToolResult)>> =
        (0..params.tool_uses.len()).map(|_| None).collect();
    let sem = if params.max_parallelism > 0 {
        Some(Arc::new(Semaphore::new(params.max_parallelism)))
    } else {
        None
    };
    let sibling_abort = CancellationToken::new();
    let mut has_error = false;

    let mut futs = FuturesUnordered::new();
    for (idx, (id, name, input)) in params.tool_uses.iter().enumerate() {
        let id = id.clone();
        let name = name.clone();
        let input = input.clone();
        let tools = params.tools.clone();
        let cwd = params.cwd.to_path_buf();
        let sid = params.session_id.to_string();
        let tid = params.turn_id.to_string();
        let turn_no = params.turn_no;
        let sem = sem.clone();
        let events = params.events.clone();
        let cancel = params.cancel.clone();
        let sibling_abort = sibling_abort.clone();

        futs.push(async move {
            let _permit = match &sem {
                Some(s) => Some(s.acquire().await.expect("semaphore closed")),
                None => None,
            };
            tokio::select! {
                _ = sibling_abort.cancelled() => None,
                _ = cancel.cancelled() => None,
                result = async {
                    let tool = tools.get(&name)?;
                    let ctx = ToolContext { cwd, session_id: sid, turn_no, sandbox: Default::default(), cancel: cancel.clone(), additional_writable_dirs: vec![], snapshot_file: None, effects: None, running_tasks: None, dangerously_disable_sandbox: true, max_file_read_bytes: 0, permission_mode: base::tool::PermissionMode::default(), config: Arc::new(base::context::EngineConfig::defaults_for("unknown")), session: Arc::new(base::context::SessionState::new(std::path::PathBuf::from("/tmp"))), tool_use_id: String::new(), agent: None, parent_messages: None, agent_depth: 0, events_tx: None };
                    match tool.call(input, ctx, ProgressSender::noop("")).await {
                        Ok(r) => {
                            let _ = events.send(AgentEvent::ToolResult {
                                id: id.clone(), name: name.clone(),
                                content: format_tool_content(&r),
                                is_error: Some(r.is_error),
                                turn_id: tid.clone(),
                            });
                            Some((idx, id, name, r))
                        }
                        Err(e) => {
                            let _ = events.send(AgentEvent::ToolResult {
                                id: id.clone(), name: name.clone(),
                                content: e.to_string(),
                                is_error: Some(true),
                                turn_id: tid.clone(),
                            });
                            Some((idx, id, name, ToolResult {
                                content: base::tool::ToolResultContent::Text(e.to_string()),
                                is_error: true, structured_content: None, mcp_meta: None, new_messages: None,
                            }))
                        }
                    }
                } => result,
            }
        });
    }

    while let Some(item) = futs.next().await {
        if let Some((idx, id, name, result)) = item {
            if result.is_error {
                has_error = true;
            }
            results[idx] = Some((id, name, result));
        }
    }

    let final_results: Vec<_> = results.into_iter().flatten().collect();
    (final_results, has_error)
}

fn format_tool_content(r: &ToolResult) -> String {
    match &r.content {
        base::tool::ToolResultContent::Text(t) => t.clone(),
        base::tool::ToolResultContent::Blocks(b) => format!("{:?}", b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::tool::InMemoryToolRegistry;
use tools::file_read::FileReadTool as ReadTool;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn dispatch_single_read() {
        let reg = InMemoryToolRegistry::new();
        reg.register(Arc::new(ReadTool));
        let (tx, _rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let tool_uses = vec![(
            "t1".into(),
            "Read".into(),
            serde_json::json!({"file_path": "Cargo.toml", "limit": 3}),
        )];
        let (results, has_error) = dispatch_tool_calls(DispatchParams {
            tools: &reg,
            tool_uses: &tool_uses,
            cwd: std::path::Path::new("."),
            session_id: "s1",
            turn_no: 1,
            turn_id: "test-turn",
            max_parallelism: 4,
            events: &tx,
            cancel: &cancel,
        })
        .await;
        assert!(!has_error);
        assert_eq!(results.len(), 1);
    }
}
