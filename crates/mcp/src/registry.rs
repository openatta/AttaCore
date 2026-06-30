//! MCP official server registry — a curated list of well-known MCP servers.
//!
//! This module provides a built-in catalog of MCP servers that users can
//! discover and add to their configuration. The registry is registered in
//! `McpManager` and accessible from the UI/CLI via `list_official_servers()`
//! and `search_official_servers(query)`.
//!
//! TS parity: `mcpRegistry.ts` (official MCP server registry).

use serde::{Deserialize, Serialize};

/// Transport type for an official MCP server entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpServerTransport {
    /// Communicates via stdin/stdout subprocess.
    Stdio,
    /// Communicates via HTTP Streamable Transport.
    StreamableHttp,
}

/// An entry in the official MCP server registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OfficialMcpServer {
    /// Short identifier (e.g. "filesystem", "github").
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// Transport type.
    pub transport: McpServerTransport,
    /// For Stdio servers: the command to invoke (e.g. "uvx", "npx").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// For Stdio servers: default arguments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// For StreamableHttp servers: the base URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// The official MCP server registry.
///
/// Provides a curated list of well-known MCP servers that users can discover
/// and add to their configuration with a single command.
#[derive(Debug, Clone)]
pub struct OfficialRegistry {
    servers: Vec<OfficialMcpServer>,
}

impl Default for OfficialRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OfficialRegistry {
    /// Build the curated list of well-known MCP servers.
    pub fn new() -> Self {
        Self {
            servers: Self::build_curated_list(),
        }
    }

    /// Return the full list of curated MCP servers.
    pub fn list_official_servers(&self) -> &[OfficialMcpServer] {
        &self.servers
    }

    /// Search the curated list by name or description.
    /// Matching is case-insensitive substring.
    pub fn search_official_servers(&self, query: &str) -> Vec<&OfficialMcpServer> {
        let q = query.to_ascii_lowercase();
        self.servers
            .iter()
            .filter(|s| {
                s.name.to_ascii_lowercase().contains(&q)
                    || s.description.to_ascii_lowercase().contains(&q)
            })
            .collect()
    }

    /// Get a specific server by exact name match.
    pub fn get_by_name(&self, name: &str) -> Option<&OfficialMcpServer> {
        self.servers.iter().find(|s| s.name == name)
    }

    fn build_curated_list() -> Vec<OfficialMcpServer> {
        vec![
            OfficialMcpServer {
                name: "filesystem".into(),
                description: "Access the local filesystem — read, write, search, and manage files and directories with configurable access controls.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("uvx".into()),
                args: vec![
                    "mcp-server-filesystem".into(),
                    "--workspace".into(),
                    "$HOME".into(),
                ],
                url: None,
            },
            OfficialMcpServer {
                name: "github".into(),
                description: "Integrate with GitHub — create and manage issues, pull requests, repositories, and search code directly from your development workflow.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("uvx".into()),
                args: vec!["mcp-server-github".into()],
                url: None,
            },
            OfficialMcpServer {
                name: "postgres".into(),
                description: "Query and explore PostgreSQL databases — inspect schemas, run read-only queries, and retrieve table metadata.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("npx".into()),
                args: vec![
                    "-y".into(),
                    "@anthropic/mcp-server-postgres".into(),
                    "--connection-string".into(),
                    "postgresql://localhost/mydb".into(),
                ],
                url: None,
            },
            OfficialMcpServer {
                name: "slack".into(),
                description: "Interact with Slack workspaces — read and send messages, manage channels, and search conversation history.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("npx".into()),
                args: vec!["@modelcontextprotocol/server-slack".into()],
                url: None,
            },
            OfficialMcpServer {
                name: "memory".into(),
                description: "Persistent knowledge graph memory — store, retrieve, and query structured information across sessions using a local knowledge graph.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("npx".into()),
                args: vec!["@modelcontextprotocol/server-memory".into()],
                url: None,
            },
            OfficialMcpServer {
                name: "puppeteer".into(),
                description: "Browser automation via Puppeteer — navigate pages, take screenshots, extract content, and interact with web applications programmatically.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("npx".into()),
                args: vec!["@anthropic/mcp-server-puppeteer".into()],
                url: None,
            },
            OfficialMcpServer {
                name: "sqlite".into(),
                description: "Query and explore SQLite databases — inspect schemas, run read-only queries, and manage database files.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("uvx".into()),
                args: vec!["mcp-server-sqlite".into(), "--db-path".into(), "$HOME/test.db".into()],
                url: None,
            },
            OfficialMcpServer {
                name: "fetch".into(),
                description: "Fetch web pages and convert them to Markdown — retrieve URLs, parse HTML content, and extract readable text for LLM consumption.".into(),
                transport: McpServerTransport::Stdio,
                command: Some("uvx".into()),
                args: vec!["mcp-server-fetch".into()],
                url: None,
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_returns_all_servers() {
        let registry = OfficialRegistry::new();
        let all = registry.list_official_servers();
        assert!(
            all.len() >= 5,
            "expected at least 5 servers, got {}",
            all.len()
        );
    }

    #[test]
    fn search_filters_by_name() {
        let registry = OfficialRegistry::new();
        let results = registry.search_official_servers("filesystem");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "filesystem");
    }

    #[test]
    fn search_filters_by_description() {
        let registry = OfficialRegistry::new();
        // "database" appears in postgres and sqlite descriptions
        let results = registry.search_official_servers("database");
        for r in &results {
            assert!(
                r.name == "postgres" || r.name == "sqlite",
                "unexpected server '{}' matched 'database'",
                r.name
            );
        }
    }

    #[test]
    fn search_is_case_insensitive() {
        let registry = OfficialRegistry::new();
        let upper = registry.search_official_servers("GITHUB");
        let lower = registry.search_official_servers("github");
        assert_eq!(upper.len(), 1);
        assert_eq!(upper.len(), lower.len());
        assert_eq!(upper[0].name, "github");
    }

    #[test]
    fn search_returns_empty_for_no_match() {
        let registry = OfficialRegistry::new();
        let results = registry.search_official_servers("xyznonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn get_by_name_finds_exact_match() {
        let registry = OfficialRegistry::new();
        let server = registry.get_by_name("postgres");
        assert!(server.is_some());
        assert_eq!(server.unwrap().name, "postgres");
    }

    #[test]
    fn get_by_name_returns_none_for_unknown() {
        let registry = OfficialRegistry::new();
        let server = registry.get_by_name("unknown");
        assert!(server.is_none());
    }

    #[test]
    fn all_servers_have_required_fields() {
        let registry = OfficialRegistry::new();
        for server in registry.list_official_servers() {
            assert!(!server.name.is_empty(), "server name is empty");
            assert!(
                !server.description.is_empty(),
                "description for '{}' is empty",
                server.name
            );
            match server.transport {
                McpServerTransport::Stdio => {
                    assert!(
                        server.command.is_some(),
                        "Stdio server '{}' has no command",
                        server.name
                    );
                }
                McpServerTransport::StreamableHttp => {
                    assert!(
                        server.url.is_some(),
                        "StreamableHttp server '{}' has no url",
                        server.name
                    );
                }
            }
        }
    }

    #[test]
    fn serde_roundtrip() {
        let registry = OfficialRegistry::new();
        let server = registry.get_by_name("github").unwrap();
        let json = serde_json::to_string(server).unwrap();
        let deserialized: OfficialMcpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "github");
        assert_eq!(deserialized.transport, McpServerTransport::Stdio);
    }
}
