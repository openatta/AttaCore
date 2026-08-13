//! Auto-generate skills from MCP server tools.
//!
//! Each MCP tool becomes a user-invocable skill with the name
//! `mcp__{server}__{tool}` so users can invoke it via `/mcp__{server}__{tool}`
//! slash command. The skill prompt explains how to use the MCP tool and
//! includes its input schema.

use base::frozen::{SkillEntry, SkillSource};
use base::interface::model::ToolDef;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

/// Get the global MCP skill body storage, lazily initialized.
fn mcp_bodies() -> &'static RwLock<HashMap<String, String>> {
    static BODIES: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();
    BODIES.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Build skill entries from a list of MCP tool definitions.
///
/// Each tool becomes a `SkillEntry` with:
/// - Name: `mcp__{server}__{tool}`
/// - Description: from the tool's description
/// - Source: `Plugin` (MCP servers are treated as plugin capabilities)
/// - Path: `(mcp:{server}:{tool})` — resolved via `mcp_skill_body()`
/// - `user_invocable: true` — visible in `/skills` list
///
/// The prompt body explains how to use the MCP tool and is stored
/// in an in-memory lookup for later expansion.
pub fn build_skills_from_mcp(server_name: &str, tools: &[ToolDef]) -> Vec<SkillEntry> {
    let mut entries = Vec::with_capacity(tools.len());

    for tool in tools {
        let skill_name = format!("mcp__{server_name}__{}", tool.name);
        let body = build_tool_prompt(server_name, tool);

        // Store the body in the in-memory map
        mcp_bodies()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(skill_name.clone(), body);

        entries.push(SkillEntry {
            name: skill_name.clone(),
            description: tool.description.clone(),
            source: SkillSource::Plugin,
            path: PathBuf::from(format!("(mcp:{server_name}:{})", tool.name)),
            user_invocable: true,
            ..Default::default()
        });
    }

    entries
}

/// Look up the stored prompt body for an MCP skill.
///
/// Called by the command expansion logic to retrieve the in-memory
/// prompt body for skills with the `(mcp:...)` path prefix.
pub fn mcp_skill_body(skill_name: &str) -> Option<String> {
    mcp_bodies()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .get(skill_name)
        .cloned()
}

/// Build the prompt body text for a single MCP tool.
fn build_tool_prompt(server_name: &str, tool: &ToolDef) -> String {
    let schema_json = serde_json::to_string_pretty(&tool.input_schema)
        .unwrap_or_else(|_| tool.input_schema.to_string());

    format!(
        r#"# MCP Tool: {server_name} / {tool_name}

{tool_description}

## Usage

This tool is provided by the **{server_name}** MCP server. Invoke it by
calling the MCP server's `{tool_name}` tool with the appropriate arguments.

## Input Schema

```json
{schema_json}
```

## Convention

When using this tool, construct a valid JSON object matching the input schema
above. Pass it as the arguments for the `{tool_name}` tool.
"#,
        server_name = server_name,
        tool_name = tool.name,
        tool_description = tool.description,
        schema_json = schema_json,
    )
}

/// Return the number of stored MCP skill bodies.
pub fn mcp_skill_count() -> usize {
    mcp_bodies().read().unwrap_or_else(|e| e.into_inner()).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool(name: &str, desc: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: desc.into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }),
        }
    }

    #[test]
    fn build_skills_creates_entries() {
        let tools = vec![
            make_tool("search", "Search the web"),
            make_tool("fetch", "Fetch a URL"),
        ];
        let entries = build_skills_from_mcp("web-search", &tools);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].name, "mcp__web-search__search");
        assert_eq!(entries[0].description, "Search the web");
        assert!(entries[0].user_invocable);
        assert_eq!(entries[0].path.to_string_lossy(), "(mcp:web-search:search)");

        assert_eq!(entries[1].name, "mcp__web-search__fetch");
    }

    #[test]
    fn mcp_skill_body_returns_stored_prompt() {
        let tools = vec![make_tool("search", "Search the web")];
        build_skills_from_mcp("test-server", &tools);

        let body = mcp_skill_body("mcp__test-server__search");
        assert!(body.is_some());
        let body = body.unwrap();
        assert!(body.contains("test-server"));
        assert!(body.contains("search"));
        assert!(body.contains("Search the web"));
        assert!(body.contains("Input Schema"));
    }

    #[test]
    fn mcp_skill_body_returns_none_for_unknown() {
        assert!(mcp_skill_body("mcp__unknown__tool").is_none());
    }

    #[test]
    fn prompt_body_contains_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch"}
            },
            "required": ["url"]
        });
        let tool = ToolDef {
            name: "fetch".into(),
            description: "Fetch a URL".into(),
            input_schema: schema,
        };
        let body = build_tool_prompt("fetcher", &tool);
        assert!(body.contains("fetch"));
        assert!(body.contains("\"url\""));
        assert!(body.contains("\"required\""));
    }
}
